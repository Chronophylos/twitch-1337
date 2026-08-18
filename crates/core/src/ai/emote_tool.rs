//! On-demand `search_emotes` chat-turn tool.
//!
//! Lets the model reach the full channel 7TV set (≈982 entries) without
//! carrying the whole catalog in every prompt. Registered only when the emote
//! provider is active (mirrors the conditional web-tool registration in
//! `command.rs`).

use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;
use serde_json::json;

use crate::twitch::seventv::SevenTvEmoteProvider;

pub const SEARCH_EMOTES_TOOL_NAME: &str = "search_emotes";

#[derive(Debug, Deserialize)]
struct SearchEmotesArgs {
    query: String,
}

pub fn search_emotes_tool() -> ToolDefinition {
    ToolDefinition {
        name: SEARCH_EMOTES_TOOL_NAME.into(),
        description: "Durchsuch das komplette 7TV-Set des Channels nach Bedeutung oder nach \
            Code-Fragment und krieg die besten Treffer zurück (exakter Code + Aussehen + Bedeutung \
            + Verwendung). Ruf das auf, wenn du ein Emote für ein Gefühl oder einen Moment willst, \
            das nicht in den Emotes dieses Turns steht — erfind niemals Emote-Codes. Maximal 20 \
            Treffer, nach Relevanz sortiert."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Freitext für das gesuchte Emote (z.B. \"lachen\", \"zustimmung\", \"sarkastischer applaus\") oder ein Teil eines Emote-Codes.",
                }
            },
            "required": ["query"]
        }),
    }
}

/// Execute a `search_emotes` call against the live emote provider for the given
/// Twitch channel. Always returns a `ToolResultMessage` — malformed args and
/// no-match cases degrade to a plain instructional string rather than erroring.
pub async fn execute_search_emotes(
    provider: &SevenTvEmoteProvider,
    twitch_channel_id: &str,
    call: &ToolCall,
) -> ToolResultMessage {
    let args = match call.parse_args::<SearchEmotesArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({"error": "invalid_arguments", "details": e.to_string()}).to_string(),
            );
        }
    };

    let query = args.query.trim();
    if query.is_empty() {
        return ToolResultMessage::for_call(
            call,
            "Provide a non-empty query describing the emote you want.".to_string(),
        );
    }

    let block = provider.search_emotes(twitch_channel_id, query).await;
    ToolResultMessage::for_call(call, block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_def_has_expected_name_and_required_query() {
        let t = search_emotes_tool();
        assert_eq!(t.name, SEARCH_EMOTES_TOOL_NAME);
        assert_eq!(t.parameters["required"][0], "query");
    }

    #[test]
    fn search_emotes_is_not_a_web_tool() {
        assert!(!crate::ai::content::is_web_tool(SEARCH_EMOTES_TOOL_NAME));
    }
}
