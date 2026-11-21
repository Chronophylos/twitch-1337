use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_core::commands::{CommandCatalog, CommandInfo, CommandKind, Permission};
use twitch_1337_web::build_router;

use super::helpers::{FakeHelix, build_state, install_crypto};

struct Catalog;

fn info(
    name: &str,
    aliases: &[&str],
    kind: CommandKind,
    permission: Permission,
    usage: &str,
) -> CommandInfo {
    CommandInfo {
        name: name.to_owned(),
        aliases: aliases.iter().map(|&a| a.to_owned()).collect(),
        kind,
        permission,
        description: format!("Beschreibung von {name}"),
        usage: usage.to_owned(),
    }
}

impl CommandCatalog for Catalog {
    fn catalog(&self) -> Vec<CommandInfo> {
        // Registry order puts the ping catch-all last; the page must group
        // commands first regardless, so a ping leads here.
        vec![
            info(
                "!kaffee",
                &[],
                CommandKind::Ping,
                Permission::Everyone,
                "!kaffee",
            ),
            info(
                "!ai",
                &["@grok"],
                CommandKind::Command,
                Permission::Everyone,
                "!ai <anweisung>",
            ),
            info(
                "!suspend",
                &[],
                CommandKind::Command,
                Permission::Moderator,
                "!suspend <befehl> [dauer]",
            ),
        ]
    }
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn app() -> (Router, twitch_1337_web::WebState) {
    install_crypto();
    let state = build_state(Arc::new(FakeHelix {
        users: HashMap::new(),
    }))
    .await;
    (build_router(state.clone()), state)
}

async fn body(res: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(res.into_body(), 256 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn commands_page_is_503_until_catalog_is_set() {
    let (app, _state) = app().await;
    let res = app.oneshot(get("/commands")).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn commands_page_is_public_and_lists_exactly_the_json_triggers() {
    let (app, state) = app().await;
    assert!(state.command_catalog.set(Arc::new(Catalog)).is_ok());

    let json_res = app.clone().oneshot(get("/commands.json")).await.unwrap();
    let doc: serde_json::Value = serde_json::from_str(&body(json_res).await).unwrap();
    let triggers: Vec<String> = doc["commands"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            std::iter::once(c["name"].as_str().unwrap().to_owned()).chain(
                c["aliases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_owned()),
            )
        })
        .collect();

    // No session cookie: the page sits in the public router group.
    let res = app.oneshot(get("/commands")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=300");
    assert!(
        res.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = body(res).await;

    for trigger in &triggers {
        assert!(
            html.contains(&format!("data-trigger>{trigger}</code>")),
            "{trigger} missing from page"
        );
    }
    assert_eq!(
        html.matches("data-trigger>").count(),
        triggers.len(),
        "page lists triggers /commands.json does not"
    );
}

#[tokio::test]
async fn commands_page_groups_commands_before_pings_and_marks_mods() {
    let (app, state) = app().await;
    assert!(state.command_catalog.set(Arc::new(Catalog)).is_ok());

    let html = body(app.oneshot(get("/commands")).await.unwrap()).await;

    let pos = |needle: &str| html.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(pos("data-trigger>!ai<") < pos("data-trigger>!kaffee<"));
    assert!(pos("data-trigger>!suspend<") < pos("data-trigger>!kaffee<"));
    assert!(html.contains("<html lang=\"de\">"));
    // Usage placeholders are escaped, descriptions shown verbatim.
    assert!(html.contains("!ai &#60;anweisung&#62;") || html.contains("!ai &lt;anweisung&gt;"));
    assert!(html.contains("Beschreibung von !suspend"));
    // One Mod badge in the legend, one on `!suspend`, none on `!ai`.
    assert_eq!(html.matches("class=\"badge-mod\"").count(), 2);
    // Standalone layout: no dashboard sidebar or auth-only links.
    assert!(!html.contains("sidebar"));
    assert!(!html.contains("/logout"));
    assert!(!html.contains("href=\"/settings"));
}
