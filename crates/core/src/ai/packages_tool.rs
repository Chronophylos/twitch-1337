//! On-demand `search_packages` chat-turn tool (Repology lookup).

use llm::{ToolCall, ToolDefinition, ToolResultMessage};
use serde::Deserialize;
use serde_json::json;

use crate::repology::{
    ALL_FAMILIES, FILTERED_SUITE_CAP, ProjectLookup, RepologyClient, collapse_hits, parse_ecosystem,
};

pub const SEARCH_PACKAGES_TOOL_NAME: &str = "search_packages";

#[derive(Debug, Deserialize)]
struct SearchPackagesArgs {
    query: String,
    #[serde(default)]
    ecosystem: Option<String>,
}

pub fn search_packages_tool() -> ToolDefinition {
    ToolDefinition {
        name: SEARCH_PACKAGES_TOOL_NAME.into(),
        description:
            "Schlag ein Software-Paket über die Linux-Paketmanager nach (AUR, Debian, Ubuntu, \
            winget). Liefert Version und Upstream-Links — erfind niemals Install-Befehle. Nutz das, \
            wenn Leute fragen, welche Version in den Repos liegt, ob es etwas überhaupt als Paket \
            gibt, oder wie ein Paket heißt. Optional grenzt ecosystem auf aur, debian, ubuntu, \
            winget, arch oder apt ein."
                .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Name des Pakets oder Projekts (z.B. ripgrep, firefox).",
                },
                "ecosystem": {
                    "type": "string",
                    "description": "Optionaler Filter: aur, arch, debian, ubuntu, winget oder apt. Weglassen, um alle v1-Families zu durchsuchen.",
                }
            },
            "required": ["query"]
        }),
    }
}

pub async fn execute_search_packages(
    client: &RepologyClient,
    call: &ToolCall,
) -> ToolResultMessage {
    let args = match call.parse_args::<SearchPackagesArgs>() {
        Ok(a) => a,
        Err(e) => {
            return ToolResultMessage::for_call(
                call,
                json!({
                    "error": "invalid_arguments",
                    "details": e.to_string(),
                })
                .to_string(),
            );
        }
    };

    let query = args.query.trim();
    if query.is_empty() {
        return ToolResultMessage::for_call(
            call,
            json!({
                "error": "invalid_arguments",
                "details": "query must not be empty",
            })
            .to_string(),
        );
    }

    let Ok(families) = parse_ecosystem(args.ecosystem.as_deref()) else {
        return ToolResultMessage::for_call(
            call,
            json!({"error": "unsupported_ecosystem"}).to_string(),
        );
    };

    let cap_per_family = if families.len() == ALL_FAMILIES.len() {
        1
    } else {
        FILTERED_SUITE_CAP
    };

    let lookup = match client.fetch_project(query).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = ?e, query, "search_packages repology lookup failed");
            return ToolResultMessage::for_call(
                call,
                json!({"error": "repology unavailable"}).to_string(),
            );
        }
    };

    let (project, packages) = match lookup {
        ProjectLookup::NotFound => {
            return ToolResultMessage::for_call(call, json!({"error": "not_found"}).to_string());
        }
        ProjectLookup::Found { name: _, packages } if packages.is_empty() => {
            return ToolResultMessage::for_call(call, json!({"error": "not_found"}).to_string());
        }
        ProjectLookup::Found { name, packages } => (name, packages),
    };

    let (ecosystem_count, hits) = collapse_hits(&project, &packages, &families, cap_per_family);

    if ecosystem_count == 0 {
        return ToolResultMessage::for_call(call, json!({"error": "not_found"}).to_string());
    }

    let hits_json: Vec<_> = hits
        .iter()
        .map(|h| {
            json!({
                "family": h.family.as_str(),
                "name": h.name,
                "version": h.version,
                "url": h.url,
            })
        })
        .collect();

    ToolResultMessage::for_call(
        call,
        json!({
            "ecosystem_count": ecosystem_count,
            "hits": hits_json,
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use llm::ToolResultMessage;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: SEARCH_PACKAGES_TOOL_NAME.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    fn test_client(server: &MockServer) -> RepologyClient {
        crate::install_crypto_provider();
        RepologyClient::with_base_url(reqwest::Client::new(), server.uri())
    }

    fn content(msg: &ToolResultMessage) -> String {
        msg.content.clone()
    }

    fn ripgrep_packages_json() -> serde_json::Value {
        serde_json::json!([
            {"repo": "aur", "visiblename": "ripgrep-git", "version": "15.1.0"},
            {"repo": "debian_11", "visiblename": "rust-ripgrep", "version": "12.1.1"},
            {"repo": "debian_14", "visiblename": "rust-ripgrep", "version": "15.2.0"},
            {"repo": "debian_unstable", "visiblename": "rust-ripgrep", "version": "15.2.0"},
            {"repo": "ubuntu_24_04", "visiblename": "rust-ripgrep", "version": "14.1.0"},
            {"repo": "ubuntu_26_10", "visiblename": "rust-ripgrep", "version": "15.2.0"},
            {"repo": "nixpkgs_unstable", "visiblename": "ripgrep", "version": "15.2.0"},
            {"repo": "winget", "visiblename": "BurntSushi.ripgrep", "version": "15.2.0"}
        ])
    }

    #[test]
    fn tool_def_has_expected_name_and_required_query() {
        let t = search_packages_tool();
        assert_eq!(t.name, SEARCH_PACKAGES_TOOL_NAME);
        assert_eq!(t.parameters["required"][0], "query");
    }

    #[test]
    fn search_packages_is_not_a_web_tool() {
        assert!(!crate::ai::content::is_web_tool(SEARCH_PACKAGES_TOOL_NAME));
    }

    #[tokio::test]
    async fn unsupported_ecosystem_returns_error_without_http() {
        let server = MockServer::start().await;
        let client = test_client(&server);

        for eco in ["npm", "docker"] {
            let msg = execute_search_packages(
                &client,
                &call(serde_json::json!({"query": "ripgrep", "ecosystem": eco})),
            )
            .await;
            let body = content(&msg);
            assert!(
                body.contains("\"error\":\"unsupported_ecosystem\""),
                "ecosystem {eco}: got {body}"
            );
        }

        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "unsupported ecosystem must not hit repology"
        );
    }

    #[tokio::test]
    async fn unfiltered_ripgrep_collapses_to_one_per_family() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/ripgrep"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ripgrep_packages_json()))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg =
            execute_search_packages(&client, &call(serde_json::json!({"query": "ripgrep"}))).await;
        let body = content(&msg);
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        let hits = parsed["hits"].as_array().expect("hits array");
        assert_eq!(parsed["ecosystem_count"], 8);
        assert!(
            hits.len() <= 4,
            "at most one hit per v1 family, got {hits:?}"
        );
        let families: Vec<_> = hits.iter().map(|h| h["family"].as_str().unwrap()).collect();
        for f in &families {
            assert!(
                matches!(*f, "aur" | "winget" | "debian" | "ubuntu"),
                "unexpected family {f}"
            );
        }
        assert_eq!(
            families
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            families.len(),
            "duplicate families: {families:?}"
        );
        assert!(hits.len() < parsed["ecosystem_count"].as_u64().unwrap() as usize);
    }

    #[tokio::test]
    async fn aur_filter_returns_only_aur_hits_full_count() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/ripgrep"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ripgrep_packages_json()))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg = execute_search_packages(
            &client,
            &call(serde_json::json!({"query": "ripgrep", "ecosystem": "aur"})),
        )
        .await;
        let body = content(&msg);
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(parsed["ecosystem_count"], 8);
        let hits = parsed["hits"].as_array().expect("hits");
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h["family"] == "aur"));
    }

    #[tokio::test]
    async fn project_404_uses_first_search_hit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/rip-grep"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/projects/"))
            .and(query_param("search", "rip-grep"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ripgrep": [{"repo": "aur", "visiblename": "ripgrep-git", "version": "1.0"}],
                "ripgrep-all": [{"repo": "aur", "visiblename": "ripgrep-all", "version": "1.0"}]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg =
            execute_search_packages(&client, &call(serde_json::json!({"query": "rip-grep"}))).await;
        let body = content(&msg);
        assert!(
            !body.contains("\"error\":\"not_found\""),
            "first search hit should resolve, got {body}"
        );
        assert!(body.contains("\"hits\""), "got {body}");
        assert!(
            body.contains("ripgrep-git") || body.contains("\"aur\""),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn project_404_empty_search_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/nope"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/projects/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg =
            execute_search_packages(&client, &call(serde_json::json!({"query": "nope"}))).await;
        let body = content(&msg);
        assert!(body.contains("\"error\":\"not_found\""), "got {body}");
    }

    #[tokio::test]
    async fn upstream_500_returns_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/ripgrep"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg =
            execute_search_packages(&client, &call(serde_json::json!({"query": "ripgrep"}))).await;
        let body = content(&msg);
        assert!(
            body.contains("\"error\":\"repology unavailable\""),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn success_has_no_install_commands() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/project/ripgrep"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ripgrep_packages_json()))
            .mount(&server)
            .await;

        let client = test_client(&server);
        let msg =
            execute_search_packages(&client, &call(serde_json::json!({"query": "ripgrep"}))).await;
        let body = content(&msg);
        assert!(!body.contains("\"install\""), "got {body}");
        assert!(!body.contains("apt install"), "got {body}");
    }
}
