//! Standalone dashboard runner — `dev-login` only.
//!
//! Spins up the web crate against a shared dev-data dir without going
//! near IRC, the OAuth callback, or a real Helix client. Lets a parallel
//! dashboard run from a worktree on a non-conflicting port (default
//! 8761) while the full bot keeps running on 8760 from the main
//! checkout.
//!
//! Env knobs:
//!   - `BIND_ADDR`  default `127.0.0.1:8761`
//!   - `DATA_DIR`   default `./dev-data`
//!   - `CHANNEL`    default `devchannel`  (cosmetic, login/denied copy only)
//!
//! Caveat: The ping actor and `MemoryStore` use atomic tmp+rename to
//! persist, so two processes against the same dir will not corrupt each
//! other's writes — but a last-writer-wins race is real. Don't run this
//! against the same dev-data the production bot is actively writing to
//! unless you're aware of that.
//!
//! OAuth client-id, public_url, and the session secret are fixed test
//! values (`WebState::for_tests`): dev-login bypasses the OAuth callback
//! entirely, so nothing here performs a real OAuth round-trip.
//!
//! Note: the fixture's fixed signed-cookie key (`0x42…` bytes) replaces the
//! previously derived one, so existing dev browser cookies are invalidated
//! once across this change but remain stable across restarts. `state.oauth`
//! redirects to the fixed `https://test.invalid` base (dev-login bypasses
//! OAuth), while `state.config.public_url` reports the real bind addr.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, WrapErr as _};
use twitch_1337_core::{install_crypto_provider, install_tracing, lifecycle::Lifecycle};
use twitch_1337_web::clock::SystemClock;
use twitch_1337_web::dev::{DEV_CSRF, DEV_SID, DEV_USER_ID, StubHelix, dev_new_session};
use twitch_1337_web::state::{WebState, WebTestConfig};
use twitch_1337_web::{bind, build_router, serve_app};

#[tokio::main]
async fn main() -> Result<()> {
    let log_ring = install_tracing();
    install_crypto_provider();

    let bind_addr: SocketAddr = env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8761".to_owned())
        .parse()
        .wrap_err("parse BIND_ADDR")?;
    let data_dir: PathBuf = env::var_os("DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./dev-data"));
    let channel = env::var("CHANNEL").unwrap_or_else(|_| "devchannel".to_owned());

    tracing::warn!(
        target: "twitch_1337_web",
        ?bind_addr, ?data_dir, channel = %channel,
        "web-dev starting (StubHelix, /_dev/login, no IRC) — DO NOT EXPOSE",
    );

    let mut state = WebState::for_tests(WebTestConfig {
        helix: Arc::new(StubHelix),
        clock: Arc::new(SystemClock),
        ping_dir: data_dir.clone(),
        memory_dir: data_dir.clone(),
        settings_dir: data_dir.clone(),
        owner: Some(DEV_USER_ID.to_owned()),
        irc_connected: true,
        channel: channel.clone(),
        bind_addr: bind_addr.to_string(),
        public_url: format!("http://{bind_addr}"),
    })
    .await?;
    // Feed the dashboard `/logs` page from this process's real subscriber.
    state.log_ring = log_ring;

    // Inject the fixed dev session (bootstrap owner via DEV_USER_ID above so
    // the role-recheck gate admits it). Stored in the session table, not
    // settings.ron, because owner is bootstrap-only.
    state
        .sessions
        .insert_with_id(DEV_SID, DEV_CSRF, dev_new_session());

    let listener = bind(bind_addr).await?;
    let lifecycle = Lifecycle::new();

    let mut app = build_router(state);
    let livereload = tower_livereload::LiveReloadLayer::new();
    let reloader = livereload.reloader();
    app = app.layer(livereload);
    install_asset_watcher(reloader);

    let mut serving = Box::pin(serve_app(listener, app, lifecycle.clone()));
    tokio::select! {
        result = &mut serving => result,
        signal = tokio::signal::ctrl_c() => {
            signal.wrap_err("register Ctrl-C handler for web-dev")?;
            lifecycle.request_graceful_stop();
            serving.await
        }
    }
}

/// Watch `assets/` + `templates/` and ping the livereload SSE on
/// change. The debouncer owns its internal worker thread; we `Box::leak`
/// it because dropping the debouncer stops the watch — leaking is the
/// honest signal that this lives for the program lifetime.
fn install_asset_watcher(reloader: tower_livereload::Reloader) {
    use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};

    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut debouncer = match new_debouncer(
        Duration::from_millis(200),
        move |res: DebounceEventResult| {
            if res.is_ok() {
                reloader.reload();
            }
        },
    ) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(target: "twitch_1337_web", error = ?e, "livereload watcher init failed");
            return;
        }
    };
    for sub in ["assets", "templates"] {
        let path = crate_dir.join(sub);
        if let Err(e) = debouncer.watcher().watch(&path, RecursiveMode::Recursive) {
            tracing::warn!(target: "twitch_1337_web", ?path, error = ?e, "livereload watch failed");
        }
    }
    Box::leak(Box::new(debouncer));
}
