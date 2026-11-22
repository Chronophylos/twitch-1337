use llm::ToolDefinition;

pub const WEB_SEARCH_TOOL_NAME: &str = "web_search";
pub const READ_URL_TOOL_NAME: &str = "read_url";

const WEB_TOOL_NAMES: &[&str] = &[WEB_SEARCH_TOOL_NAME, READ_URL_TOOL_NAME];

pub fn is_web_tool(name: &str) -> bool {
    WEB_TOOL_NAMES.contains(&name)
}

pub fn web_search_tool() -> ToolDefinition {
    ToolDefinition {
        name: WEB_SEARCH_TOOL_NAME.into(),
        description: "Websuche für alles, was du nicht sicher weißt: aktuelles Geschehen, \
Zahlen, Releases, Preise, oder Leute und Projekte, die dir nichts sagen. Lieber suchen als raten \
— dein Wissen hat einen Stichtag, der Chat redet über heute. Liefert kurze Treffer mit URLs; \
zum Reinschauen dann read_url."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Suchanfrage."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 10, "description": "Anzahl Treffer (1–10). Weglassen für den Default."}
            },
            "required": ["query"]
        }),
    }
}

pub fn read_url_tool() -> ToolDefinition {
    ToolDefinition::derived::<super::executor::ReadUrlArgs>(
        READ_URL_TOOL_NAME,
        "Öffne eine URL und schau nach, was da ist: Bilder, Video, Audio, PDFs, Webseiten, \
JSON. Nutz das immer, wenn jemand einen Link postet — gerade bei Kurzlinks wie kappa.lol oder \
i.nuuls.com, deren Inhalt du der URL nicht ansiehst. Optional `instruction` für eine konkrete \
Frage zum Inhalt; ohne bekommst du eine vollständige Beschreibung.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_tool_is_routed_as_web_tool() {
        let names: Vec<String> = [web_search_tool(), read_url_tool()]
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, WEB_TOOL_NAMES);
    }
}
