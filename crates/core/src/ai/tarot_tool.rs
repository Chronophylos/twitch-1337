use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use rand::rng;
use serde::Deserialize;
use serde_json::json;

use crate::tarot::{Orientation, READING_GUIDE, SpreadKind, draw_three_card, parse_spread};

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
        description: "Wenn jemand ein Tarot-Reading oder einen Card Pull will. Zieht einen Spread \
            und liefert Karten, Bedeutungen und eine kurze Leseanleitung. Lieber dieses Tool als \
            Karten zu erfinden."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "spread": {
                    "type": "string",
                    "description": "Named spread. v1 only \"three_card\" (default)."
                },
                "question": {
                    "type": "string",
                    "description": "Optionale Frage der fragenden Person. Wird im Result wiederholt, ändert den Shuffle nicht."
                }
            }
        }),
    }
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
        Ok(SpreadKind::ThreeCard) => SpreadKind::ThreeCard,
        Err(_) => {
            return ToolResultMessage::for_call(
                call,
                json!({"error": "unsupported_spread"}).to_string(),
            );
        }
    };
    let _ = spread;

    let draw = draw_three_card(&mut rng());
    let positions: Vec<_> = draw
        .positions
        .iter()
        .map(|p| {
            json!({
                "label": p.label,
                "id": p.card.id,
                "card": p.card.name,
                "orientation": match p.orientation {
                    Orientation::Upright => "upright",
                    Orientation::Reversed => "reversed",
                },
                "meaning": p.meaning(),
            })
        })
        .collect();

    let mut body = json!({
        "spread": "three_card",
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
        assert!(v["guide"].as_str().unwrap().contains("Past"));
        assert!(!v["positions"][0]["meaning"].as_str().unwrap().is_empty());
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
        c.arguments = json!("nope");
        // Force deserialize failure via parse_args path: set arguments to a
        // non-object that DrawTarotArgs rejects, or use arguments_parse_error.
        c.arguments = json!({"spread": 123});
        let msg = execute_draw_tarot(&c).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "invalid_arguments");
    }
}
