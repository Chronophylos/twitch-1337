use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use rand::RngExt;
use random_flight::{Direction, FlightRequest};
use serde::Deserialize;
use serde_json::json;
use twitch_1337_data::flight::{Aircraft, Airport};

use crate::commands::{CommandCancellation, CommandTaskGroup};
use crate::util::parse_flight_duration;

pub const PLAN_FLIGHT_TOOL_NAME: &str = "plan_flight";

#[derive(Debug, Deserialize)]
struct PlanFlightArgs {
    #[serde(default)]
    aircraft: Option<String>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    departure: Option<String>,
    #[serde(default)]
    arrival: Option<String>,
    #[serde(default)]
    towards: Option<String>,
}

/// The ICAO type codes the baked aircraft DB knows, as a comma-separated list.
/// Shared by the tool schema and the `unknown_aircraft` error so the two can
/// never drift from the DB or from each other.
fn known_aircraft() -> Vec<&'static str> {
    twitch_1337_data::flight::aircraft()
        .iter()
        .map(|a| a.icao_type)
        .collect()
}

pub fn plan_flight_tool() -> ToolDefinition {
    ToolDefinition {
        name: PLAN_FLIGHT_TOOL_NAME.into(),
        description: "Würfelt einen Flugplan für Flugsimulator-Leute: echte Flughäfen, passend \
zum Flugzeug und zur gewünschten Blockzeit, inklusive SimBrief-Link. Das ist derselbe Generator \
wie der !fl Command. Nutz das, wenn jemand nach einem Flug, einer Route oder \"was soll ich \
fliegen\" fragt. Start und Himmelsrichtung sind optional festlegbar. Will jemand eine bestimmte \
Strecke (\"EDDF nach London\"), setz departure und arrival: dann wird genau diese Strecke geplant \
und duration entfällt. Ohne arrival wird das Ziel gewürfelt. Achtung: towards ist nur die \
Richtung, nicht das Ziel - \"Richtung Berlin\" landet irgendwo auf dieser Peilung, nicht in \
Berlin. Für \"von X nach Berlin\" nimm departure + arrival; ohne Start geht nur towards."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "aircraft": {
                    "type": "string",
                    "description": format!(
                        "ICAO-Typcode des Flugzeugs. Bekannt sind nur: {}. Weglassen für ein zufälliges Flugzeug.",
                        known_aircraft().join(", "),
                    )
                },
                "duration": {
                    "type": "string",
                    "description": "Gewünschte Blockzeit als h/m-String, z.B. \"1h\", \"90m\", \"2h30m\". Pflicht, außer wenn departure und arrival beide gesetzt sind."
                },
                "departure": {
                    "type": "string",
                    "description": "ICAO- oder IATA-Code des Startflughafens, z.B. EDFE. Weglassen für einen zufälligen Start."
                },
                "arrival": {
                    "type": "string",
                    "description": "ICAO- oder IATA-Code des Zielflughafens, z.B. EGLL. Nur zusammen mit departure. Dann sind duration und towards egal."
                },
                "towards": {
                    "type": "string",
                    "description": "Grobe Flugrichtung. Entweder eine Himmelsrichtung (N, NE, E, SE, S, SW, W, NW bzw. north/nord, osten, ...) oder der ICAO/IATA-Code eines Flughafens, in dessen Richtung geflogen werden soll - für \"Richtung Berlin\" also EDDB. Weglassen für eine zufällige Richtung."
                }
            },
            "required": []
        }),
    }
}

/// Compass words to bearings. German included, the chat is German.
fn cardinal_bearing(s: &str) -> Option<f64> {
    let b = match s.to_lowercase().as_str() {
        "n" | "north" | "nord" | "norden" => 0.0,
        "ne" | "northeast" | "nordost" | "nordosten" => 45.0,
        "e" | "east" | "ost" | "osten" => 90.0,
        "se" | "southeast" | "suedost" | "südost" | "suedosten" | "südosten" => 135.0,
        "s" | "south" | "sued" | "süd" | "sueden" | "süden" => 180.0,
        "sw" | "southwest" | "suedwest" | "südwest" | "suedwesten" | "südwesten" => 225.0,
        "w" | "west" | "westen" => 270.0,
        "nw" | "northwest" | "nordwest" | "nordwesten" => 315.0,
        _ => return None,
    };
    Some(b)
}

fn resolve_direction(towards: &str) -> Option<Direction> {
    if let Some(bearing) = cardinal_bearing(towards) {
        return Some(Direction::Bearing(bearing));
    }
    let airport = twitch_1337_data::location::lookup_by_icao(towards)
        .or_else(|| twitch_1337_data::location::lookup_by_iata(towards))?;
    Some(Direction::Towards {
        lat: f64::from(airport.lat),
        lon: f64::from(airport.lon),
    })
}

fn err(call: &ToolCall, value: serde_json::Value) -> ToolResultMessage {
    ToolResultMessage::for_call(call, value.to_string())
}

pub async fn execute_plan_flight(
    call: &ToolCall,
    cancellation: &CommandCancellation,
) -> ToolResultMessage {
    let args = match call.parse_args::<PlanFlightArgs>() {
        Ok(a) => a,
        Err(e) => {
            return err(
                call,
                json!({"error": "invalid_arguments", "details": e.to_string()}),
            );
        }
    };

    let trimmed = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };

    let aircraft = match trimmed(&args.aircraft) {
        Some(code) => match twitch_1337_data::flight::aircraft_by_icao_type(&code) {
            Some(a) => Some(a),
            None => {
                return err(
                    call,
                    json!({
                        "error": "unknown_aircraft",
                        "aircraft": code,
                        "known": known_aircraft(),
                    }),
                );
            }
        },
        None => None,
    };

    let departure = match trimmed(&args.departure) {
        None => None,
        Some(code) => match resolve_airport(&code) {
            Some(a) => Some(a),
            None => {
                return err(
                    call,
                    json!({"error": "unknown_departure", "departure": code}),
                );
            }
        },
    };

    if let Some(arrival) = trimmed(&args.arrival) {
        let Some(departure) = departure else {
            return err(call, json!({"error": "arrival_needs_departure"}));
        };
        let Some(arrival) = resolve_airport(&arrival) else {
            return err(
                call,
                json!({"error": "unknown_arrival", "arrival": arrival}),
            );
        };
        return execute_fixed_route(call, departure, arrival, aircraft);
    }
    let departure = departure.map(|a| a.icao);

    let aircraft = aircraft.unwrap_or_else(|| {
        let db = twitch_1337_data::flight::aircraft();
        &db[rand::rng().random_range(0..db.len())]
    });

    let Some(block_time) = args.duration.as_deref().and_then(parse_flight_duration) else {
        return err(
            call,
            json!({"error": "invalid_duration", "duration": args.duration}),
        );
    };

    let direction = match trimmed(&args.towards) {
        None => None,
        Some(towards) => match resolve_direction(&towards) {
            Some(d) => Some(d),
            None => {
                return err(call, json!({"error": "unknown_target", "towards": towards}));
            }
        },
    };

    // Retries internally, can block for a while. The command task group owns
    // and joins this child; command execution has no detached fallback.
    if cancellation.is_cancelled() {
        return err(call, json!({"error": "cancelled"}));
    }
    let Some(group) = CommandTaskGroup::current() else {
        tracing::error!("plan_flight called without an active command task group");
        return err(call, json!({"error": "task_group_unavailable"}));
    };
    let receiver = match group.spawn_blocking(move || {
        random_flight::generate_flight_plan(&FlightRequest {
            aircraft,
            block_time,
            departure,
            direction,
        })
    }) {
        Ok(receiver) => receiver,
        Err(error) => {
            crate::log_handled!(warn, error, "plan_flight could not register blocking child");
            return err(call, json!({"error": "task_group_closed"}));
        }
    };
    let result = receiver.await;

    let fp = match result {
        Ok(Ok(fp)) => fp,
        Ok(Err(e)) => {
            tracing::debug!(error = ?e, "plan_flight: no route found");
            return route_error(call, &e);
        }
        Err(e) => {
            crate::log_handled!(warn, e, "plan_flight: generation task panicked");
            return err(call, json!({"error": "generation_failed"}));
        }
    };

    ToolResultMessage::for_call(call, plan_json(&fp).to_string())
}

/// One error vocabulary for both paths, so the model sees the same code for
/// the same failure.
fn route_error(call: &ToolCall, e: &random_flight::Error) -> ToolResultMessage {
    use random_flight::Error as E;
    let code = match e {
        E::UnknownAirport { .. } => "unknown_departure",
        E::RunwayTooShort { .. } => "runway_too_short",
        E::RangeExceeded { .. } => "range_exceeded",
        E::NoRouteInDirection => "no_route_in_direction",
        E::TargetIsDeparture => "target_is_departure",
        E::BlockTimeTooLong { max } => {
            return err(
                call,
                json!({
                    "error": "impossible_duration",
                    "achievable_max": crate::cooldown::format_duration_hm(*max),
                    "details": e.to_string(),
                }),
            );
        }
        _ => "no_route_found",
    };
    err(call, json!({"error": code, "details": e.to_string()}))
}

/// ICAO first (the flight DB's key), then IATA via the location DB, which
/// maps it back to an ICAO.
fn resolve_airport(code: &str) -> Option<&'static Airport> {
    twitch_1337_data::flight::find_by_icao(code).or_else(|| {
        twitch_1337_data::location::lookup_by_iata(code)?
            .icao
            .and_then(twitch_1337_data::flight::find_by_icao)
    })
}

/// Departure and arrival are both pinned: no dice, just validate and compute.
/// plan_route checks runway and range before computing a profile, so even
/// trying every aircraft type is cheap enough to run inline.
fn execute_fixed_route(
    call: &ToolCall,
    departure: &'static Airport,
    arrival: &'static Airport,
    aircraft: Option<&'static Aircraft>,
) -> ToolResultMessage {
    if std::ptr::eq(departure, arrival) {
        return route_error(call, &random_flight::Error::TargetIsDeparture);
    }
    let result = match aircraft {
        Some(ac) => random_flight::plan_route(departure, arrival, ac),
        None => {
            let mut fits: Vec<_> = twitch_1337_data::flight::aircraft()
                .iter()
                .filter_map(|ac| random_flight::plan_route(departure, arrival, ac).ok())
                .collect();
            if fits.is_empty() {
                // Types fail for different reasons (range for the short-field
                // ones, runway for the heavies), so no single reason is true.
                return err(
                    call,
                    json!({
                        "error": "no_aircraft_fits",
                        "departure": departure.icao,
                        "arrival": arrival.icao,
                        "details": "no known aircraft clears both runways and has the range",
                    }),
                );
            }
            Ok(fits.swap_remove(rand::rng().random_range(0..fits.len())))
        }
    };

    match result {
        Ok(fp) => ToolResultMessage::for_call(call, plan_json(&fp).to_string()),
        Err(e) => route_error(call, &e),
    }
}

fn plan_json(fp: &random_flight::FlightPlan) -> serde_json::Value {
    let bearing = random_flight::geo::initial_bearing(
        fp.departure.latitude,
        fp.departure.longitude,
        fp.arrival.latitude,
        fp.arrival.longitude,
    );

    json!({
        "aircraft": fp.aircraft.icao_type,
        "aircraft_name": fp.aircraft.name,
        "departure": fp.departure.icao,
        "arrival": fp.arrival.icao,
        "distance_nm": fp.distance_nm.round(),
        "block_time": crate::cooldown::format_duration_hm(fp.block_time),
        "cruise_flight_level": fp.cruise_altitude_ft / 100,
        "direction": random_flight::geo::cardinal_direction(bearing),
        "bearing_deg": bearing.round(),
        "simbrief_url": fp.simbrief_url(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: PLAN_FLIGHT_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    async fn run(args: serde_json::Value) -> serde_json::Value {
        let cancellation = CommandCancellation::new();
        let group = CommandTaskGroup::new();
        let msg = group
            .scope(execute_plan_flight(&call(args), &cancellation))
            .await;
        group.join_all().await.expect("plan_flight task group");
        serde_json::from_str(&msg.content).expect("tool result is json")
    }

    #[tokio::test]
    async fn pinned_aircraft_and_departure_are_used() {
        let v = run(json!({"aircraft": "B738", "duration": "1h", "departure": "EDDF"})).await;
        assert_eq!(v["aircraft"], "B738", "got: {v}");
        assert_eq!(v["departure"], "EDDF", "got: {v}");
        assert!(v["simbrief_url"].as_str().unwrap().contains("simbrief"));
    }

    #[tokio::test]
    async fn omitted_aircraft_picks_a_known_type() {
        let v = run(json!({"duration": "1h"})).await;
        let picked = v["aircraft"].as_str().expect("aircraft");
        assert!(
            twitch_1337_data::flight::aircraft()
                .iter()
                .any(|ac| ac.icao_type == picked),
            "got: {v}"
        );
    }

    #[tokio::test]
    async fn cardinal_towards_steers_the_route() {
        let v = run(json!({
            "aircraft": "B738", "duration": "1h", "departure": "EDDF", "towards": "east"
        }))
        .await;
        assert_eq!(v["direction"], "E");
    }

    #[tokio::test]
    async fn airport_towards_steers_the_route() {
        let v = run(json!({
            "aircraft": "B738", "duration": "1h", "departure": "EDDF", "towards": "EDDB"
        }))
        .await;
        let eddf = twitch_1337_data::flight::find_by_icao("EDDF").expect("EDDF");
        let eddb = twitch_1337_data::location::lookup_by_icao("EDDB").expect("EDDB");
        let wanted = random_flight::geo::initial_bearing(
            eddf.latitude,
            eddf.longitude,
            f64::from(eddb.lat),
            f64::from(eddb.lon),
        );
        let got = v["bearing_deg"].as_f64().expect("bearing_deg");
        let diff = random_flight::geo::bearing_difference(got, wanted);
        assert!(
            diff <= random_flight::DIRECTION_TOLERANCE_DEG + 0.5,
            "bearing {got} is {diff} deg off the bearing to EDDB ({wanted})"
        );
    }

    #[test]
    fn compass_words_fold_case_including_umlauts() {
        // to_ascii_lowercase() leaves U+00DC alone, so "SÜDEN" used to miss the
        // table and fall through to the airport lookup as an unknown target.
        for (word, want) in [
            ("SÜDEN", 180.0),
            ("Südost", 135.0),
            ("SÜDWESTEN", 225.0),
            ("Nord", 0.0),
            ("EAST", 90.0),
        ] {
            assert_eq!(cardinal_bearing(word), Some(want), "word: {word}");
        }
        assert_eq!(cardinal_bearing("nowhere"), None);
    }

    #[test]
    fn tool_schema_lists_every_known_aircraft() {
        let def = plan_flight_tool();
        let listed = def.parameters["properties"]["aircraft"]["description"]
            .as_str()
            .expect("aircraft description");
        for ac in twitch_1337_data::flight::aircraft() {
            assert!(
                listed.contains(ac.icao_type),
                "{} missing from the tool schema: {listed}",
                ac.icao_type
            );
        }
    }

    #[tokio::test]
    async fn unknown_target_errors() {
        let v = run(json!({"aircraft": "B738", "duration": "1h", "towards": "nowhere"})).await;
        assert_eq!(v["error"], "unknown_target");
    }

    #[tokio::test]
    async fn target_equal_to_departure_errors() {
        let v = run(json!({
            "aircraft": "B738", "duration": "1h", "departure": "EDDF", "towards": "EDDF"
        }))
        .await;
        assert_eq!(v["error"], "target_is_departure");
    }

    #[tokio::test]
    async fn unknown_departure_errors() {
        let v = run(json!({"aircraft": "B738", "duration": "1h", "departure": "ZZZZ"})).await;
        assert_eq!(v["error"], "unknown_departure");
    }

    #[tokio::test]
    async fn unknown_aircraft_errors() {
        let v = run(json!({"aircraft": "XXXX", "duration": "1h"})).await;
        assert_eq!(v["error"], "unknown_aircraft");
    }

    #[tokio::test]
    async fn fixed_route_ignores_duration_and_links_simbrief() {
        let v = run(json!({"aircraft": "B738", "departure": "EDDF", "arrival": "EGLL"})).await;
        assert_eq!(
            (&v["departure"], &v["arrival"]),
            (&json!("EDDF"), &json!("EGLL"))
        );
        let url = v["simbrief_url"].as_str().expect("simbrief_url");
        assert!(
            url.contains("orig=EDDF") && url.contains("dest=EGLL"),
            "{url}"
        );
    }

    #[tokio::test]
    async fn fixed_route_without_aircraft_picks_one_that_reaches() {
        // Long-haul: rules out the short-range types, so a pick must check range.
        for _ in 0..20 {
            let v = run(json!({"departure": "EDDF", "arrival": "KJFK"})).await;
            assert_eq!(v["arrival"], "KJFK", "got: {v}");
        }
    }

    #[tokio::test]
    async fn fixed_route_errors() {
        for (args, want) in [
            (
                json!({"aircraft": "B738", "departure": "EDDF", "arrival": "YSSY"}),
                "range_exceeded",
            ),
            (
                json!({"departure": "EDDF", "arrival": "ZZZZ"}),
                "unknown_arrival",
            ),
            (
                json!({"departure": "ZZZZ", "arrival": "EDDF"}),
                "unknown_departure",
            ),
            (
                json!({"departure": "EDDF", "arrival": "EDDF"}),
                "target_is_departure",
            ),
            (json!({"arrival": "EGLL"}), "arrival_needs_departure"),
        ] {
            let v = run(args.clone()).await;
            assert_eq!(v["error"], want, "args: {args}, got: {v}");
        }
    }

    #[tokio::test]
    async fn fixed_route_accepts_iata() {
        let v = run(json!({"aircraft": "B738", "departure": "FRA", "arrival": "LHR"})).await;
        assert_eq!(v["departure"], "EDDF", "got: {v}");
        assert_eq!(v["arrival"], "EGLL", "got: {v}");
    }

    #[tokio::test]
    async fn fixed_route_nobody_can_fly_says_so() {
        // The DB's shortest strip is below every type's minimum runway.
        let short = twitch_1337_data::flight::airports()
            .iter()
            .min_by_key(|a| a.runway_length_ft)
            .expect("airports");
        assert!(
            twitch_1337_data::flight::aircraft()
                .iter()
                .all(|ac| short.runway_length_ft < ac.min_runway_length_ft)
        );
        let v = run(json!({"departure": "EDDF", "arrival": short.icao})).await;
        assert_eq!(v["error"], "no_aircraft_fits", "got: {v}");
    }

    #[tokio::test]
    async fn missing_duration_on_random_route_errors() {
        let v = run(json!({"aircraft": "B738"})).await;
        assert_eq!(v["error"], "invalid_duration");
    }

    #[tokio::test]
    async fn invalid_duration_errors() {
        let v = run(json!({"aircraft": "B738", "duration": "soon"})).await;
        assert_eq!(v["error"], "invalid_duration");
    }
}
