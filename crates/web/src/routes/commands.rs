//! Public, unauthenticated command list: `GET /commands.json` for chat-client
//! autocomplete (format in `docs/commands-json.md`) and `GET /commands`, the
//! same chat catalog as a German help page, plus a separate Viewer-Facing
//! Tool section fed by Aurora's executable registration availability.

use std::sync::{Arc, OnceLock};

use askama::Template as _;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use twitch_1337_core::ai::viewer_tools::{AuroraToolCatalog, ViewerToolInfo};
use twitch_1337_core::commands::{CommandCatalog, CommandInfo, CommandKind, Permission};

type CatalogCell = Arc<OnceLock<Arc<dyn CommandCatalog>>>;

const CACHE_CONTROL: &str = "public, max-age=300";

#[derive(Serialize)]
struct CommandsDoc<'a> {
    version: u32,
    bot: &'static str,
    channel: &'a str,
    lang: &'static str,
    commands: Vec<CommandInfo>,
}

pub fn router(catalog: CatalogCell, channel: Arc<str>, aurora_tools: AuroraToolCatalog) -> Router {
    let page_catalog = catalog.clone();
    let page_channel = channel.clone();
    let page_aurora_tools = aurora_tools;
    Router::new()
        .route(
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
                ([(header::CACHE_CONTROL, CACHE_CONTROL)], Json(doc)).into_response()
            }),
        )
        .route(
            "/commands",
            get(move || async move {
                let Some(catalog) = page_catalog.get() else {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
                page(&page_channel, catalog.catalog(), &page_aurora_tools)
            }),
        )
}

#[derive(askama::Template)]
#[template(path = "commands/page.html")]
struct CommandsPage<'a> {
    channel: &'a str,
    commands: Vec<CommandInfo>,
    pings: Vec<CommandInfo>,
    aurora_tools: Vec<ViewerToolInfo>,
}

fn page(channel: &str, catalog: Vec<CommandInfo>, aurora_tools: &AuroraToolCatalog) -> Response {
    let (commands, pings) = catalog
        .into_iter()
        .partition(|info| info.kind == CommandKind::Command);
    match (CommandsPage {
        channel,
        commands,
        pings,
        aurora_tools: aurora_tools.enabled_metadata().collect(),
    })
    .render()
    {
        Ok(body) => ([(header::CACHE_CONTROL, CACHE_CONTROL)], Html(body)).into_response(),
        Err(error) => {
            tracing::error!(target: "twitch_1337_web", ?error, "commands page render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}
