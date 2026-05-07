use llm::ToolDefinition;

/// Names of all tools registered by [`ai_tools`]. Used by the chat-turn
/// executor to dispatch web tool calls back to this module.
pub const WEB_TOOL_NAMES: &[&str] = &["web_search", "fetch_url"];

pub fn is_web_tool(name: &str) -> bool {
    WEB_TOOL_NAMES.contains(&name)
}

pub fn ai_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "web_search".into(),
            description:
                "Search the web for current information and return concise results with URLs."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 10}
                },
                "required": ["query"]
            }),
        },
        ToolDefinition::derived::<super::executor::FetchUrlArgs>(
            "fetch_url",
            "Fetch a URL and return extracted readable plain text content.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names() -> Vec<String> {
        ai_tools().into_iter().map(|t| t.name).collect()
    }

    #[test]
    fn ai_tools_surface_contains_only_web_tools() {
        let names = tool_names();
        assert_eq!(names, vec!["web_search", "fetch_url"]);
        assert!(!names.iter().any(|n| n.ends_with("_memory")));
        assert!(!names.iter().any(|n| n == "save_memory"));
        assert!(!names.iter().any(|n| n == "merge_memories"));
    }
}
