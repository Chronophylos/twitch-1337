//! Embedded web dashboard for the twitch-1337 bot.
//!
//! Public surfaces:
//! - `/healthz` (always public)
//! - `/commands.json` (always public, chat-client autocomplete)
//! - `/commands` (always public, German help page from the same catalog)
//! - `/login`, `/auth/callback`, `/logout` (OAuth + post-login `?next=` deep-link)
//! - `/assets/*` (embedded htmx + pico bundles, immutable cache)
//!
//! Auth tiers:
//! - **Viewer** (allowlisted Twitch login): `/`, `/pings` (read), `/leaderboard`, `/flights`.
//!   Read-only — non-GET/HEAD methods are rejected by `viewer_method_guard`.
//! - **Mod** (broadcaster / hidden admins / Twitch moderators): pings mutations,
//!   `/memory/{soul,lore,users,state}` CRUD, schedules, emotes.
//! - **Owner** (bootstrap `[twitch].owner`): `/settings`, `/ai-profiles`, `/logs`.

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
use axum::http::{HeaderName, HeaderValue, header};
use eyre::{Result, WrapErr as _};
use tokio::net::TcpListener;
use tower_cookies::CookieManagerLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use twitch_1337_core::lifecycle::Lifecycle;

pub use crate::state::WebState;

const SESSION_PERSIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Sent on every response. Scripts and styles come only from `/assets/*`:
/// templates carry no inline `<script>`, `<style>`, `style=""` or `on*=`.
/// `img-src` admits Twitch avatars (Helix `profile_image_url`); no other
/// remote image is rendered. The Twitch login is a link to `/auth/start`,
/// which redirects; no form posts off-origin, so `form-action` stays `'self'`.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
     img-src 'self' https://static-cdn.jtvnw.net; \
     style-src 'self'; script-src 'self'; connect-src 'self'; \
     frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// Security headers set on every response, assets and error pages included.
const SECURITY_HEADERS: [(HeaderName, &str); 3] = [
    (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "same-origin"),
];

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
    let server = tokio::spawn(serve_app(listener, app, lifecycle));
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

/// Every Dashboard Tier lands on `/pings`, the first sidebar entry, so the
/// landing page and the nav order agree for Viewers too.
async fn root_redirect() -> axum::response::Redirect {
    axum::response::Redirect::to("/pings")
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
        .route_layer(axum::middleware::from_fn_with_state(
            mod_state.clone(),
            auth::require_mod,
        ))
        .with_state(mod_state);

    let owner_state = state.clone();
    let owner_only = Router::new()
        .merge(routes::settings::owner_router())
        .merge(routes::ai_profiles::router())
        .merge(routes::logs::router())
        .route_layer(axum::middleware::from_fn_with_state(
            owner_state.clone(),
            auth::require_owner,
        ))
        .with_state(owner_state);

    let app = public
        .merge(viewer)
        .merge(mod_only)
        .merge(owner_only)
        .layer(axum::middleware::from_fn(error::htmx_errors))
        .layer(CookieManagerLayer::new());
    SECURITY_HEADERS
        .into_iter()
        .fold(app, |app, (name, value)| {
            app.layer(SetResponseHeaderLayer::overriding(
                name,
                HeaderValue::from_static(value),
            ))
        })
        .layer(TraceLayer::new_for_http())
}
