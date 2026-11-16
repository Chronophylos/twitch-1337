//! Embedded web dashboard for the twitch-1337 bot.
//!
//! Public surfaces:
//! - `/healthz` (always public)
//! - `/commands.json` (always public, chat-client autocomplete)
//! - `/login`, `/auth/callback`, `/logout` (OAuth + post-login `?next=` deep-link)
//! - `/assets/*` (embedded htmx + pico bundles, immutable cache)
//!
//! Auth tiers:
//! - **Viewer** (allowlisted Twitch login): `/`, `/pings` (read), `/leaderboard`, `/flights`.
//!   Read-only — non-GET/HEAD methods are rejected by `viewer_method_guard`.
//! - **Mod** (broadcaster / hidden admins / helix moderators): pings mutations,
//!   `/memory/{soul,lore,users,state}` CRUD, stubs.

pub mod auth;
pub mod clock;
pub mod config;
#[cfg(feature = "dev-login")]
pub mod dev;
pub mod error;
pub mod flash;
pub mod helix;
pub mod nav;
pub mod routes;
pub mod state;
pub mod user_facing;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use eyre::{Result, WrapErr as _};
use tokio::net::TcpListener;
use tower_cookies::CookieManagerLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use twitch_1337_core::lifecycle::Lifecycle;

pub use crate::state::WebState;

const SESSION_PERSIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

pub struct WebDeps {
    pub bind_addr: SocketAddr,
    pub state: WebState,
}

/// Bind synchronously so a port-in-use failure aborts startup (loud) rather
/// than silently degrading the spawned task.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .wrap_err_with(|| format!("bind {addr}"))
}

/// Spawn the dashboard and its session-persistence loop with one shared,
/// latched lifecycle. Returning both handles is intentional: the core
/// supervisor adopts them as direct siblings, so a server or session-writer
/// fault wakes the other and a teardown timeout aborts and joins each actual
/// handle.
pub fn spawn_web_runtime(
    listener: TcpListener,
    deps: WebDeps,
    lifecycle: Lifecycle,
) -> twitch_1337_core::WebRuntimeTasks {
    let sessions = deps.state.sessions.clone();
    let app = build_router(deps.state);
    let session_persistence = tokio::spawn(persist_sessions(sessions, lifecycle.clone()));
    let server = tokio::spawn(async move {
        if let Err(error) = serve_app(listener, app, lifecycle).await {
            panic!("web server exited with error: {error:?}");
        }
    });
    twitch_1337_core::WebRuntimeTasks {
        server,
        session_persistence,
    }
}

/// Persist sliding session activity on the same lifecycle as the web server.
/// The returned pre-spawned handle is part of [`WebRuntimeTasks`](twitch_1337_core::WebRuntimeTasks).
async fn persist_sessions(sessions: Arc<auth::session::SessionTable>, lifecycle: Lifecycle) {
    let mut tick = tokio::time::interval(SESSION_PERSIST_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return,
            _ = tick.tick() => {
                // Operation boundary: lifecycle wins over a simultaneously
                // ready tick. An already-started atomic persist is allowed to
                // finish before this future returns.
                if lifecycle.is_cancelled() {
                    return;
                }
                sessions.persist().await;
            }
        }
    }
}

/// Serve a pre-built router with graceful shutdown.
pub async fn serve_app(listener: TcpListener, app: Router, lifecycle: Lifecycle) -> Result<()> {
    let url = listener
        .local_addr()
        .map(|a| format!("http://{a}/"))
        .unwrap_or_else(|_| "<unknown>".to_owned());
    info!(
        target: "twitch_1337_web",
        build = routes::health::BUILD_NUM,
        sha = routes::health::GIT_SHA,
        "Build info",
    );
    info!(target: "twitch_1337_web", %url, "Web dashboard listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            lifecycle.wait_for_cancellation().await;
        })
        .await
        .wrap_err("web serve")?;
    warn!(target: "twitch_1337_web", "Web dashboard stopped");
    Ok(())
}

async fn root_redirect(
    axum::extract::Extension(session): axum::extract::Extension<auth::session::Session>,
) -> axum::response::Redirect {
    match session.role {
        // Owner gets the same default landing as Mod — settings is one step
        // beyond, but the management surface is shared.
        auth::role::Role::Owner | auth::role::Role::Mod => axum::response::Redirect::to("/pings"),
        auth::role::Role::Viewer => axum::response::Redirect::to("/leaderboard"),
    }
}

pub fn build_router(state: WebState) -> Router {
    #[cfg_attr(
        not(feature = "dev-login"),
        expect(unused_mut, reason = "only the dev-login block mutates the router")
    )]
    let mut public = Router::new()
        .merge(routes::health::router(state.irc_connected.clone()))
        .merge(routes::assets::router())
        .merge(routes::commands::router(
            state.command_catalog.clone(),
            state.channel.clone(),
        ))
        .merge(auth::auth_router().with_state(state.clone()));
    #[cfg(feature = "dev-login")]
    {
        public = public.merge(dev::router(state.clone()));
    }

    let viewer_state = state.clone();
    let viewer = Router::new()
        .route("/", axum::routing::get(root_redirect))
        .merge(routes::pings::viewer_router())
        .merge(routes::leaderboard::router())
        .merge(routes::flights::viewer_router())
        .layer(axum::middleware::from_fn(auth::viewer_method_guard))
        .route_layer(axum::middleware::from_fn_with_state(
            viewer_state.clone(),
            |s, c, r, n| auth::require_role(auth::role::Role::Viewer, s, c, r, n),
        ))
        .with_state(viewer_state);

    let mod_state = state.clone();
    let mod_only = Router::new()
        .merge(routes::pings::mod_router())
        .merge(routes::flights::mod_router())
        .merge(routes::memory::router())
        .merge(routes::schedules::router())
        .merge(routes::emotes::router())
        .merge(routes::stubs::router())
        .route_layer(axum::middleware::from_fn_with_state(
            mod_state.clone(),
            auth::require_mod,
        ))
        .with_state(mod_state);

    let owner_state = state.clone();
    let owner_only = Router::new()
        .merge(routes::settings::owner_router())
        .merge(routes::ai_profiles::router())
        .route_layer(axum::middleware::from_fn_with_state(
            owner_state.clone(),
            auth::require_owner,
        ))
        .with_state(owner_state);

    public
        .merge(viewer)
        .merge(mod_only)
        .merge(owner_only)
        .layer(axum::middleware::from_fn(error::htmx_errors))
        .layer(CookieManagerLayer::new())
        .layer(TraceLayer::new_for_http())
}
