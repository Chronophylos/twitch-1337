use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use rand::rng;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tarot::{
    Arcana, DrawnCard, Orientation, SpreadKind, draw_single_card, draw_three_card, guide_for,
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
        description: "Tarot ziehen statt Karten erfinden. Zuerst den richtigen Spread wählen:\n\
            - Tageskarte / \"für heute\" / daily → spread \"single\" (question optional).\n\
            - Rat zu einem Thema / Blockade / \"was soll ich tun\" → \"guidance\" \
(Ursache, Hindernis, Rat). question Pflicht.\n\
            - Zukunft / Verlauf / \"was kommt\" → \"future\" (Vergangenheit, Gegenwart, Zukunft; \
default). question Pflicht.\n\
            Alias: \"three_card\" → future. Die guide im Result gilt nur für den gewählten Spread."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "spread": {
                    "type": "string",
                    "description": "\"single\" | \"guidance\" | \"future\" (default). Alias: \"three_card\" → future."
                },
                "question": {
                    "type": "string",
                    "description": "Frage / Thema der Asker-Person. Pflicht bei guidance und future; bei single optional. Ändert den Shuffle nicht."
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

fn normalized_question(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|s| !s.is_empty())
}

fn question_required(spread: SpreadKind) -> bool {
    matches!(spread, SpreadKind::Guidance | SpreadKind::Future)
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

    let Ok(spread) = parse_spread(args.spread.as_deref()) else {
        return ToolResultMessage::for_call(
            call,
            json!({"error": "unsupported_spread"}).to_string(),
        );
    };

    let question = normalized_question(args.question.as_deref());
    if question_required(spread) && question.is_none() {
        return ToolResultMessage::for_call(
            call,
            json!({
                "error": "missing_question",
                "spread": spread.as_str(),
                "details": "guidance and future require a non-empty question",
            })
            .to_string(),
        );
    }

    let mut rng = rng();
    let positions = match spread.three_card_labels() {
        None => {
            let draw = draw_single_card(&mut rng);
            vec![position_json(&draw.card)]
        }
        Some(labels) => {
            let draw = draw_three_card(&mut rng, labels);
            draw.positions.iter().map(position_json).collect()
        }
    };

    let mut body = json!({
        "spread": spread.as_str(),
        "positions": positions,
        "guide": guide_for(spread),
    });
    if let Some(q) = question {
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

    #[tokio::test]
    async fn empty_args_require_question_for_default_future() {
        let msg = execute_draw_tarot(&call(json!({}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "missing_question");
        assert_eq!(v["spread"], "future");
    }

    #[tokio::test]
    async fn future_with_question_returns_timeline() {
        let msg = execute_draw_tarot(&call(json!({
            "spread": "future",
            "question": "was kommt beruflich?"
        })))
        .await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["spread"], "future");
        assert_eq!(v["question"], "was kommt beruflich?");
        assert_eq!(v["positions"].as_array().unwrap().len(), 3);
        assert_eq!(v["positions"][0]["label"], "Vergangenheit");
        assert_eq!(v["positions"][1]["label"], "Gegenwart");
        assert_eq!(v["positions"][2]["label"], "Zukunft");
        assert_eq!(v["guide"].as_str().unwrap(), guide_for(SpreadKind::Future));
    }

    #[tokio::test]
    async fn guidance_requires_question() {
        let msg = execute_draw_tarot(&call(json!({"spread": "guidance"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["error"], "missing_question");
    }

    #[tokio::test]
    async fn guidance_with_question_uses_cause_obstacle_advice_labels() {
        let msg = execute_draw_tarot(&call(json!({
            "spread": "guidance",
            "question": "jobwechsel?"
        })))
        .await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["spread"], "guidance");
        assert_eq!(v["positions"][0]["label"], "Ursache");
        assert_eq!(v["positions"][1]["label"], "Hindernis");
        assert_eq!(v["positions"][2]["label"], "Rat");
        assert_eq!(
            v["guide"].as_str().unwrap(),
            guide_for(SpreadKind::Guidance)
        );
    }

    #[tokio::test]
    async fn single_spread_returns_one_day_card_without_question() {
        let msg = execute_draw_tarot(&call(json!({"spread": "single"}))).await;
        let v: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(v["spread"], "single");
        assert!(v.get("question").is_none());
        assert_eq!(v["positions"].as_array().unwrap().len(), 1);
        assert_eq!(v["positions"][0]["label"], "Day");
        assert_eq!(v["guide"].as_str().unwrap(), guide_for(SpreadKind::Single));
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
