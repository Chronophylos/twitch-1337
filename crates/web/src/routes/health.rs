use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use serde_json::json;

pub const GIT_SHA: &str = env!("GIT_SHA_SHORT");
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn router(irc_connected: Arc<AtomicBool>) -> Router {
    Router::new()
        .route(
            "/healthz",
            get(move || {
                let flag = irc_connected.clone();
                async move {
                    if flag.load(Ordering::Relaxed) {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        )
        .route(
            "/version",
            get(|| async {
                Json(json!({
                    "version": PKG_VERSION,
                    "sha": GIT_SHA,
                }))
            }),
        )
}
