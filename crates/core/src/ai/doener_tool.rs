use llm::ToolDefinition;
use serde::Deserialize;

pub const DOENER_TOOL_NAME: &str = "doener_index";

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct DoenerArgs {
    #[serde(default)]
    pub city: Option<String>,
}

pub fn doener_tool() -> ToolDefinition {
    ToolDefinition {
        name: DOENER_TOOL_NAME.into(),
        description: "Look up the German Döner price index from dönerindex.com. \
            Without `city`, returns the country-wide aggregate (location count, \
            avg/min/max price). With `city` (free-form), returns the top matching \
            cities and their per-city aggregate. Use this for any question about \
            Döner prices, kebab prices, or how expensive Döner is in a German city."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "Optional city name or prefix. German spelling preferred (e.g. 'Köln', 'München')."
                }
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_def_has_expected_name() {
        let t = doener_tool();
        assert_eq!(t.name, "doener_index");
    }

    #[test]
    fn doener_index_is_not_a_web_tool() {
        // Regression guard: a future refactor must not gate this tool behind
        // [ai.web]. is_web_tool drives routing into ContentToolExecutor.
        assert!(!crate::ai::content::is_web_tool(DOENER_TOOL_NAME));
    }
}
