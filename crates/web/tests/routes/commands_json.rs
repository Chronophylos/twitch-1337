use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_core::commands::{CommandCatalog, CommandInfo, CommandKind, Permission};
use twitch_1337_web::build_router;

use super::helpers::{FakeHelix, build_state, install_crypto};

struct OneCommand;

impl CommandCatalog for OneCommand {
    fn catalog(&self) -> Vec<CommandInfo> {
        vec![CommandInfo {
            name: "!lb".to_owned(),
            aliases: vec![],
            kind: CommandKind::Command,
            permission: Permission::Everyone,
            description: "1337-Leaderboard, Top 5".to_owned(),
            usage: "!lb".to_owned(),
        }]
    }
}

fn get() -> Request<Body> {
    Request::builder()
        .uri("/commands.json")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn commands_json_is_public_and_503_until_catalog_is_set() {
    install_crypto();
    let state = build_state(Arc::new(FakeHelix {
        users: HashMap::new(),
    }))
    .await;
    let cell = state.command_catalog.clone();
    let app = build_router(state);

    let res = app.clone().oneshot(get()).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

    assert!(cell.set(Arc::new(OneCommand)).is_ok());
    let res = app.oneshot(get()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=300");
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(res.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(body["version"], 1);
    assert_eq!(body["lang"], "de");
    assert_eq!(body["commands"][0]["name"], "!lb");
    assert_eq!(body["commands"][0]["permission"], "everyone");
}
