//! Embedded web dashboard for the twitch-1337 bot. v1 surfaces:
//! /healthz only; auth + ping + memory routes land in later tasks.

pub mod error;
pub mod routes;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use eyre::{Result, WrapErr as _};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::{info, warn};

pub struct WebDeps {
    pub bind_addr: SocketAddr,
    pub irc_connected: Arc<AtomicBool>,
}

pub async fn run_web(deps: WebDeps, shutdown: Arc<Notify>) -> Result<()> {
    let app = routes::health::router(deps.irc_connected);
    let listener = TcpListener::bind(deps.bind_addr)
        .await
        .wrap_err_with(|| format!("bind {}", deps.bind_addr))?;
    info!(target: "twitch_1337_web", addr = %deps.bind_addr, "Web dashboard listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.notified().await })
        .await
        .wrap_err("web serve")?;
    warn!(target: "twitch_1337_web", "Web dashboard stopped");
    Ok(())
}
