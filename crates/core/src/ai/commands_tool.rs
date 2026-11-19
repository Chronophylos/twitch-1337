//! `list_commands` chat-turn tool: the live command catalog, same source as
//! `/commands.json`, so the model answers "wie geht X?" from the registry
//! instead of inventing commands.

use std::sync::{Arc, OnceLock};

use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde_json::json;

use crate::commands::CommandCatalog;

pub const LIST_COMMANDS_TOOL_NAME: &str = "list_commands";

/// Shared cell filled by `run_bot` once the registry exists; the `!ai`
/// command is itself in the registry, so it cannot hold the catalog directly.
pub type CatalogCell = Arc<OnceLock<Arc<dyn CommandCatalog>>>;

pub fn list_commands_tool() -> ToolDefinition {
    ToolDefinition {
        name: LIST_COMMANDS_TOOL_NAME.into(),
        description: "Liste aller aktiven Chat-Befehle des Bots (Name, Aliase, Berechtigung, \
            Beschreibung, Usage). Ruf das auf, bevor du jemandem einen Befehl erklärst oder \
            empfiehlst — erfind niemals Befehle oder Syntax."
            .into(),
        parameters: json!({ "type": "object", "properties": {} }),
    }
}

pub fn execute_list_commands(cell: &CatalogCell, call: &ToolCall) -> ToolResultMessage {
    let body = match cell.get() {
        Some(catalog) => json!({ "commands": catalog.catalog() }).to_string(),
        None => json!({ "error": "catalog_unavailable" }).to_string(),
    };
    ToolResultMessage::for_call(call, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{CommandInfo, CommandKind, Permission};

    struct One;
    impl CommandCatalog for One {
        fn catalog(&self) -> Vec<CommandInfo> {
            vec![CommandInfo {
                name: "!lb".into(),
                aliases: vec![],
                kind: CommandKind::Command,
                permission: Permission::Everyone,
                description: "Leaderboard".into(),
                usage: "!lb".into(),
            }]
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: LIST_COMMANDS_TOOL_NAME.into(),
            arguments: json!({}),
            arguments_parse_error: None,
        }
    }

    #[test]
    fn lists_catalog_or_reports_unavailable() {
        let cell = CatalogCell::default();
        assert!(
            execute_list_commands(&cell, &call())
                .content
                .contains("catalog_unavailable")
        );
        assert!(cell.set(Arc::new(One)).is_ok());
        let out: serde_json::Value =
            serde_json::from_str(&execute_list_commands(&cell, &call()).content).unwrap();
        assert_eq!(out["commands"][0]["name"], "!lb");
        assert_eq!(out["commands"][0]["permission"], "everyone");
    }
}
