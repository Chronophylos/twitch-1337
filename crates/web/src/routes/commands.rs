//! `GET /commands.json`: public, unauthenticated command list for chat-client
//! autocomplete. Format documented in `docs/commands-json.md`.

use std::sync::{Arc, OnceLock};

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use twitch_1337_core::commands::{CommandCatalog, CommandInfo};

type CatalogCell = Arc<OnceLock<Arc<dyn CommandCatalog>>>;

#[derive(Serialize)]
struct CommandsDoc<'a> {
    version: u32,
    bot: &'static str,
    channel: &'a str,
    lang: &'static str,
    commands: Vec<CommandInfo>,
}

pub fn router(catalog: CatalogCell, channel: Arc<str>) -> Router {
    Router::new().route(
        "/commands.json",
        get(move || async move {
            let Some(catalog) = catalog.get() else {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            let doc = CommandsDoc {
                version: 1,
                bot: "twitch-1337",
                channel: &channel,
                lang: "de",
                commands: catalog.catalog(),
            };
            response(doc)
        }),
    )
}

fn response(doc: CommandsDoc<'_>) -> Response {
    ([(header::CACHE_CONTROL, "public, max-age=300")], Json(doc)).into_response()
}
