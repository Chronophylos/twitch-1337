//! Tool args + definitions for the v2 chat-turn and dreamer loops.

use schemars::JsonSchema;
use serde::Deserialize;

use llm::ToolDefinition;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    pub path: String,
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteStateArgs {
    pub slug: String,
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteStateArgs {
    pub slug: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SayArgs {
    pub text: String,
}

pub fn chat_turn_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::derived::<WriteFileArgs>(
            "write_file",
            "Overwrite a memory file (SOUL.md, LORE.md, or users/<id>.md). Body is the new full prose body; frontmatter is store-managed. Permission-gated by speaker role.",
        ),
        ToolDefinition::derived::<WriteStateArgs>(
            "write_state",
            "Create or overwrite a state file at state/<slug>.md. slug is lowercase a–z 0–9 dashes, ≤64 chars.",
        ),
        ToolDefinition::derived::<DeleteStateArgs>(
            "delete_state",
            "Remove a state file. Regulars may only delete state files they created.",
        ),
        ToolDefinition::derived::<SayArgs>(
            "say",
            "Append one chat line. Aim for ≤3 sentences per call; the app truncates >500 chars to ≤500 + …. Multiple calls produce multiple lines.",
        ),
    ]
}

pub fn dreamer_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::derived::<WriteFileArgs>(
            "write_file",
            "Overwrite SOUL.md / LORE.md / users/<id>.md.",
        ),
        ToolDefinition::derived::<WriteStateArgs>("write_state", "Overwrite state/<slug>.md."),
        ToolDefinition::derived::<DeleteStateArgs>("delete_state", "Remove a stale state file."),
    ]
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    #[test]
    fn chat_turn_tools_has_four_named_tools() {
        let names: Vec<_> = chat_turn_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec!["write_file", "write_state", "delete_state", "say"]
        );
    }

    #[test]
    fn dreamer_tools_has_three_no_say() {
        let names: Vec<_> = dreamer_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["write_file", "write_state", "delete_state"]);
    }

    #[test]
    fn write_file_args_round_trip() {
        let v = serde_json::json!({"path": "users/12.md", "body": "hi"});
        let parsed: WriteFileArgs = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.path, "users/12.md");
        assert_eq!(parsed.body, "hi");
    }
}
