use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use rand::rng;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tarot::{
    Arcana, DrawnCard, Orientation, READING_GUIDE, SpreadKind, draw_single_card, draw_three_card,
    parse_spread,
};

pub const TAROT_TOOL_NAME: &str = "draw_tarot";

#[derive(Debug, Deserialize)]
struct DrawTarotArgs {
    #[serde(default)]
    spread: Option<String>,
    #[serde(default)]
    question: Option<String>,
}

pub fn tarot_tool() -> ToolDefinition {
    ToolDefinition {
        name: TAROT_TOOL_NAME.into(),
        description: "Wenn jemand Tarot will: Tageskarte / cards for today → spread \"single\". \
            Allgemeine Legung oder drei Karten → \"three_card\" (default). Liefert deutsche Namen, \
            Bedeutungen und Leseanleitung. Lieber dieses Tool als Karten zu erfinden. Antwortstil \
            steht in der guide im Result (Tageskarte vs Drei-Karten unterscheiden sich)."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "spread": {
                    "type": "string",
                    "description": "\"single\" (Tageskarte) oder \"three_card\" (default)."
                },
                "question": {
                    "type": "string",
                    "description": "Optionale Frage der fragenden Person. Wird im Result wiederholt, ändert den Shuffle nicht."
                }
            }
        }),
    }
}

fn arcana_json(card: &crate::tarot::Card) -> &'static str {
    match card.arcana {
        Arcana::Major => "major",
        Arcana::Minor => "minor",
    }
}

fn position_json(p: &DrawnCard) -> Value {
    json!({
        "label": p.label,
        "id": p.card.id,
        "card": p.card.name,
        "arcana": arcana_json(p.card),
        "orientation": match p.orientation {
            Orientation::Upright => "upright",
            Orientation::Reversed => "reversed",
        },
        "meaning": p.meaning(),
    })
}

pub async fn execute_draw_tarot(call: &ToolCall) -> ToolResultMessage {
    let args = match call.parse_args::<DrawTarotArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({"error": "invalid_arguments", "details": e.to_string()}).to_string(),
            );
        }
    };

    let spread = match parse_spread(args.spread.as_deref()) {
        Ok(s) => s,
        Err(_) => {
            return ToolResultMessage::for_call(
                call,
                json!({"error": "unsupported_spread"}).to_string(),
            );
        }
    };

    let mut rng = rng();
    let (spread_name, positions) = match spread {
        SpreadKind::ThreeCard => {
            let draw = draw_three_card(&mut rng);
            (
                "three_card",
                draw.positions.iter().map(position_json).collect::<Vec<_>>(),
            )
        }
        SpreadKind::Single => {
            let draw = draw_single_card(&mut rng);
            ("single", vec![position_json(&draw.card)])
        }
    };

    let mut body = json!({
        "spread": spread_name,
        "positions": positions,
        "guide": READING_GUIDE,
    });
    if let Some(q) = args
        .question
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        body["question"] = json!(q);
    }

    ToolResultMessage::for_call(call, body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::ToolCall;
    use serde_json::json;

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: TAROT_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    #[test]
    fn tool_def_name() {
        assert_eq!(tarot_tool().name, TAROT_TOOL_NAME);
    }

    #[tokio::test]
    async fn empty_args_returns_three_positions_and_guide() {
        let msg = execute_draw_tarot(&call(json!({}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["spread"], "three_card");
        assert!(v.get("question").is_none());
        assert_eq!(v["positions"].as_array().unwrap().len(), 3);
        assert_eq!(v["positions"][0]["label"], "Past");
        assert!(v["positions"][0].get("arcana").is_some());
        assert!(v["guide"].as_str().unwrap().contains("Past"));
        assert!(!v["positions"][0]["meaning"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn single_spread_returns_one_day_card() {
        let msg = execute_draw_tarot(&call(json!({"spread": "single"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["spread"], "single");
        assert_eq!(v["positions"].as_array().unwrap().len(), 1);
        assert_eq!(v["positions"][0]["label"], "Day");
        assert!(matches!(
            v["positions"][0]["arcana"].as_str(),
            Some("major") | Some("minor")
        ));
        assert!(v["guide"].as_str().unwrap().contains("single"));
    }

    #[tokio::test]
    async fn question_is_echoed() {
        let msg = execute_draw_tarot(&call(json!({"question": "pass the exam?"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["question"], "pass the exam?");
    }

    #[tokio::test]
    async fn unsupported_spread_errors() {
        let msg = execute_draw_tarot(&call(json!({"spread": "celtic_cross"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "unsupported_spread");
    }

    #[tokio::test]
    async fn malformed_args_invalid_arguments() {
        let mut c = call(json!({}));
        c.arguments = json!({"spread": 123});
        let msg = execute_draw_tarot(&c).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "invalid_arguments");
    }
}
