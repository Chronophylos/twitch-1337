use llm::ToolDefinition;

const WEB_TOOL_NAMES: &[&str] = &["web_search", "read_url"];

pub fn is_web_tool(name: &str) -> bool {
    WEB_TOOL_NAMES.contains(&name)
}

pub fn ai_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "web_search".into(),
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
        },
        ToolDefinition::derived::<super::executor::ReadUrlArgs>(
            "read_url",
            "Öffne eine URL und schau nach, was da ist: Bilder, Video, Audio, PDFs, Webseiten, \
JSON. Nutz das immer, wenn jemand einen Link postet — gerade bei Kurzlinks wie kappa.lol oder \
i.nuuls.com, deren Inhalt du der URL nicht ansiehst. Optional `instruction` für eine konkrete \
Frage zum Inhalt; ohne bekommst du eine vollständige Beschreibung.",
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
    fn ai_tools_surface_contains_search_and_read() {
        let names = tool_names();
        assert_eq!(names, vec!["web_search", "read_url"]);
    }

    #[test]
    fn read_url_does_not_appear_under_old_name() {
        let names = tool_names();
        assert!(!names.iter().any(|n| n == "fetch_url"));
    }
}
