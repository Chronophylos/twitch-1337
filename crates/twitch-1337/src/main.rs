use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use color_eyre::eyre::Result;
use eyre::{WrapErr as _, eyre};
use jiff::Timestamp;
use secrecy::ExposeSecret as _;
use tokio::sync::oneshot;
use tracing::{Instrument as _, info, info_span};
use twitch_1337_core::{
    AuthenticatedLoginCredentials, PersonalBest, PersonalWorst, Services,
    ai::chat_context::ChatContext,
    ai::memory::store::MemoryStore,
    aviation, doener, ensure_data_dir, get_data_dir, install_crypto_provider, install_tracing,
    llm_factory, load_configuration, load_hall_of_shame, load_leaderboard,
    ping::{PingManager, ping_runtime},
    run_bot, setup_and_verify_twitch_client,
    suspend::SuspensionManager,
    twitch::mouth_setup::build_mouths,
    twitch::seventv::build_emote_provider,
    twitch::whisper,
    util::clock::SystemClock,
    util::observability::metrics,
};
use twitch_1337_web::helix::{AccessTokenProvider, HelixClient as _, ReqwestHelixClient};
use twitch_irc::login::LoginCredentials as _;

use twitch_1337_core as twitch_1337;

fn build_aviation_client(
    bootstrap: Option<&twitch_1337::config::AviationstackBootstrap>,
    settings: &twitch_1337::settings::AviationstackSettings,
) -> Option<aviation::AviationClient> {
    if settings.enabled && bootstrap.is_none() {
        tracing::warn!(
            "aviationstack.enabled=true in settings but no [aviationstack].api_key in \
             config.toml; metadata enrichment disabled. Set the api_key or disable in \
             /settings → Aviationstack."
        );
    }

    match aviation::AviationClient::new() {
        Ok(client) => {
            let client = if settings.enabled {
                client.with_aviationstack(
                    bootstrap.map(|b| b.api_key.clone()),
                    settings.base_url.clone(),
                    settings.timeout_secs,
                )
            } else {
                client
            };
            Some(client)
        }
        Err(e) => {
            tracing::error!(
                error = ?e,
                "Failed to initialize aviation client; aviation commands and flight tracker disabled"
            );
            None
        }
    }
}

const HELIX_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const HELIX_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn helix_http_client() -> Result<reqwest::Client> {
    helix_http_client_with_timeout(HELIX_REQUEST_TIMEOUT)
}

fn helix_http_client_with_timeout(request_timeout: std::time::Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(HELIX_CONNECT_TIMEOUT)
        .timeout(request_timeout)
        .build()
        .wrap_err("build Helix HTTP client")
}

#[tokio::main]
pub async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        // reqwest uses rustls-no-provider; the ring default provider must be
        // installed before any TLS handshake or `Client::builder().build()`
        // panics. Skipping color_eyre + tracing on the healthcheck path is
        // intentional — they aren't needed for a one-shot HTTP probe.
        install_crypto_provider();
        return run_healthcheck().await;
    }

    color_eyre::install()?;
    let observability = install_tracing();
    install_crypto_provider();
    let config = load_configuration().await?;
    ensure_data_dir().await?;
    twitch_1337_core::util::persist::sweep_stale_tmp(&get_data_dir());
    // Dashboard-managed runtime settings. Opened before IRC connect so the
    // settings-store value `admin_channel` can be passed to the IRC setup,
    // which needs it to join channels at startup.
    // The same Arc-backed store is shared with both the IRC handlers (via
    // `Services.settings`) and `WebState`.  The audit log lives at
    // `$DATA_DIR/settings_audit.log`.
    let audit_log: Arc<dyn twitch_1337::settings::AuditLog> = Arc::new(
        twitch_1337::settings::FileAuditLog::new(get_data_dir().join("settings_audit.log")),
    );
    let (settings_store, settings_handle) = twitch_1337::settings::SettingsStore::open(
        &get_data_dir(),
        audit_log,
        &config.twitch.channel,
    )
    .wrap_err("Failed to open settings store")?;
    // `[otel]` / `[langfuse]` absent or broken leaves that export off; the
    // bot runs the same. Enabled before `startup` opens: a span created while
    // the OTel traces filter is still `off` is never exported, and neither
    // would its tree be.
    let otel = observability.enable_otel(
        config.otel.as_ref(),
        config.langfuse.as_ref(),
        &settings_handle,
    );

    // `startup` root: opened once tracing and export are up, handed to
    // `run_bot`, which closes it once the handlers are spawned and the bot
    // runs.
    let startup = info_span!(parent: None, "startup");
    let Prepared {
        client,
        incoming,
        services,
        ping_runtime,
        aviation_tracker_rx,
        web_spawner,
    } = prepare(
        config,
        settings_store,
        settings_handle,
        observability.log_ring(),
    )
    .instrument(startup.clone())
    .await?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut bot_task = tokio::spawn(run_bot(
        client,
        incoming,
        services,
        ping_runtime,
        aviation_tracker_rx,
        web_spawner,
        startup,
        shutdown_rx,
    ));

    let outcome = tokio::select! {
        biased;
        result = &mut bot_task => join_bot_task(result),
        signal = wait_for_shutdown_signal() => {
            if let Err(error) = signal {
                // Signal registration failure is still a process fault, but do
                // not abort `run_bot`: request its normal drain first, join it,
                // and only then return the adapter error to the process.
                if let Err(shutdown_error) = request_shutdown(shutdown_tx) {
                    info!(error = ?shutdown_error, "Bot task already stopped before shutdown request");
                }
                match bot_task.await {
                    Ok(Err(bot_error)) => Err(bot_error),
                    Ok(Ok(())) => Err(error),
                    Err(join_error) => Err(eyre::eyre!("bot task join failed after signal error: {join_error}")),
                }
            } else {
                match request_shutdown(shutdown_tx) {
                    Ok(()) => join_bot_task(bot_task.await),
                    Err(signal_error) => {
                        // The bot finished before the signal request was sent. Its
                        // own result is authoritative when it failed; otherwise
                        // retain the signal-adapter error.
                        match bot_task.await {
                            Ok(Err(bot_error)) => Err(bot_error),
                            Ok(Ok(())) => Err(signal_error),
                            Err(join_error) => Err(eyre::eyre!("bot task join failed after signal race: {join_error}")),
                        }
                    }
                }
            }
        }
    };
    // Last, so the bot's own shutdown lines are exported too.
    if let Some(otel) = otel {
        otel.shutdown().await;
    }
    outcome
}

/// Everything `run_bot` needs, built inside the `startup` trace.
struct Prepared {
    client: Arc<twitch_1337::AuthenticatedTwitchClient>,
    incoming: tokio::sync::mpsc::UnboundedReceiver<twitch_irc::message::ServerMessage>,
    services: Services,
    ping_runtime: twitch_1337::ping::PingRuntime,
    aviation_tracker_rx: Option<tokio::sync::mpsc::Receiver<aviation::TrackerCommand>>,
    web_spawner: Option<twitch_1337::WebSpawner>,
}

/// Startup up to `run_bot`, after config and settings are loaded: IRC
/// connect and client builds.
async fn prepare(
    config: twitch_1337::config::Configuration,
    settings_store: Arc<twitch_1337::settings::SettingsStore>,
    settings_handle: twitch_1337::settings::SettingsHandle,
    log_ring: Arc<twitch_1337::util::log_ring::LogRing>,
) -> Result<Prepared> {
    let local = twitch_1337::util::to_berlin(Timestamp::now());
    let initial_schedule_count = settings_handle.load().schedules.len();
    info!(
        local_time = ?local,
        utc_time = ?Timestamp::now(),
        channel = %config.twitch.channel,
        username = %config.twitch.username,
        schedule_count = initial_schedule_count,
        "Starting twitch-1337 bot"
    );

    // Restart-required channel list for IRC channel joins.
    let extra_channels: Vec<String> = settings_handle
        .load()
        .twitch
        .admin_channel
        .clone()
        .into_iter()
        .collect();

    let (incoming, client, credentials, bot_user_id) =
        setup_and_verify_twitch_client(&config, &extra_channels).await?;
    let client = Arc::new(client);

    let doener_client = Arc::new(
        doener::DoeneratlasClient::new().wrap_err("Failed to initialize Döneratlas client")?,
    );
    let whisper_credentials = credentials.clone();
    let whisper = whisper::HelixWhisperSender::new(
        whisper_credentials,
        config.twitch.client_id.expose_secret().to_string(),
        bot_user_id,
        get_data_dir(),
    )
    .await
    .map(|sender| Arc::new(sender) as Arc<dyn whisper::WhisperSender>)?;

    let irc_connected = Arc::new(AtomicBool::new(false));

    let ping_manager =
        PingManager::load(&get_data_dir()).wrap_err("Failed to load ping manager")?;
    let (ping_runtime, ping_handle, ping_names_rx) = ping_runtime(ping_manager);

    // Build aviation client from settings (restart-required fields).
    // `enabled`, `base_url`, and `timeout_secs` come from the settings store;
    // `api_key` remains in config.toml as a secret and is not dashboard-managed.
    let aviation_client = {
        let av_settings = settings_handle.load().aviationstack.clone();
        build_aviation_client(config.aviationstack.as_ref(), &av_settings)
    };

    // AMP: same invariant as DHL below, log and disable, never abort startup.
    let amp_client = match config.amp.as_ref() {
        Some(a) => match twitch_1337_core::amp::AmpClient::new(
            a.url.clone(),
            a.username.clone(),
            a.password.clone(),
        ) {
            Ok(c) => Some(Arc::new(c)),
            Err(error) => {
                tracing::error!(?error, "failed to build AMP client, !servers disabled");
                None
            }
        },
        None => None,
    };

    // Finnhub: same invariant, log and disable, never abort startup.
    let finnhub_client = match config.finnhub.as_ref() {
        Some(f) => match twitch_1337_core::finnhub::FinnhubClient::new(f.api_key.clone()) {
            Ok(c) => Some(Arc::new(c)),
            Err(error) => {
                tracing::error!(?error, "failed to build Finnhub client, !aktien disabled");
                None
            }
        },
        None => None,
    };

    // DHL needs no settings lookup, only the config key. Match the aviation
    // invariant: log and disable on failure, never abort startup.
    let dhl_client = match config.dhl.as_ref() {
        Some(d) => match twitch_1337_core::dhl::DhlClient::new(d.api_key.clone()) {
            Ok(c) => Some(Arc::new(c)),
            Err(error) => {
                tracing::error!(
                    ?error,
                    "failed to build DHL client, track_shipment disabled"
                );
                None
            }
        },
        None => None,
    };

    // ipinfo only enriches lookup_network; RDAP keeps the tool working
    // without it. Same invariant: log and disable, never abort startup.
    let ipinfo_client = match config.ipinfo.as_ref() {
        Some(i) => match twitch_1337_core::ipinfo::IpinfoClient::new(i.token.clone()) {
            Ok(c) => Some(Arc::new(c)),
            Err(error) => {
                tracing::error!(
                    ?error,
                    "failed to build ipinfo client, lookup_network runs on RDAP only"
                );
                None
            }
        },
        None => None,
    };

    let llm = Arc::new(llm_factory::LlmRegistry::new(
        config.ai.as_ref(),
        settings_handle.clone(),
    ));

    let recap_handle = if config.ai.is_some() {
        Some(Arc::new(
            twitch_1337::ai::recap::RecapHandle::load(get_data_dir()).await,
        ))
    } else {
        None
    };

    // Memory v2 store opens unconditionally so the dashboard editor has a
    // handle even when `[ai]` is disabled. The same `Arc`-backed store is
    // shared with the bot's IRC handlers / dreamer ritual via `Services`.
    let memory_store = MemoryStore::open(&get_data_dir(), settings_handle.clone())
        .await
        .wrap_err("open memory store")?;

    // Build the v2 memory bundle (store + transcript) once at composition;
    // `run_bot` reads it from `Services`. `None` when AI is off.
    let ai_memory =
        twitch_1337::ai::memory::build_ai_memory_v2(config.ai.is_some(), memory_store.clone())
            .await
            .wrap_err("build ai memory v2")?;

    // Bind before the (opt-in, up to ~20 s) history prefill so /healthz
    // connections queue on the accept backlog instead of being refused
    // while Docker's HEALTHCHECK start-period is running.
    let web_listener = if config.web.enabled {
        let bind_addr: std::net::SocketAddr = config
            .web
            .bind_addr
            .parse()
            .wrap_err("parse web.bind_addr")?;
        Some(twitch_1337_web::bind(bind_addr).await?)
    } else {
        None
    };

    // Shared chat-history buffers: built once here so `!ai`, `!news`,
    // `!haiku`, the dispatcher, and the recap batcher all clone the same
    // buffers out of `Services`. `ChatContext::build` may fetch history
    // prefill (up to ~20 s when `ai.prefill` is on); overlap it with the
    // mouths' Helix I/O instead of serializing both ahead of run_bot.
    let data_dir = get_data_dir();
    let (chat_ctx, (mouths, echo)) = tokio::join!(
        ChatContext::build(
            &settings_handle,
            config.ai.is_some(),
            &config.twitch.channel,
            recap_handle.as_deref(),
        ),
        build_mouths(&config, &data_dir),
    );

    let suspension = Arc::new(SuspensionManager::new());
    let started_at = std::time::Instant::now();

    // Load the leaderboard before building the web spawner so the same Arc
    // can be shared with WebState (dashboard read) and the IRC tracker (writes).
    let leaderboard: Arc<tokio::sync::RwLock<HashMap<String, PersonalBest>>> = Arc::new(
        tokio::sync::RwLock::new(load_leaderboard(&get_data_dir()).await),
    );

    let hall_of_shame: Arc<tokio::sync::RwLock<HashMap<String, PersonalWorst>>> = Arc::new(
        tokio::sync::RwLock::new(load_hall_of_shame(&get_data_dir()).await),
    );

    // Pre-create the flight-tracker mpsc channel when aviation is enabled so
    // the sender Arc can be wired into WebState before the handlers are spawned.
    let (aviation_tracker_tx, aviation_tracker_rx) = if aviation_client.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<aviation::TrackerCommand>(32);
        (Some(Arc::new(tx)), Some(rx))
    } else {
        (None, None)
    };

    // Build the schedule runtime store once so the IRC orchestrator and WebState share the same Arc.
    let schedule_runtime = twitch_1337::schedule::ScheduleRuntimeStore::open(&get_data_dir());

    // Soft-fail: open/migrate error leaves None and disables the tracker only.
    let emote_tracker = twitch_1337::emote_tracker::try_open(&get_data_dir());

    // 7TV emote grounding: enabled only when AI + `ai.emotes` + tracker are
    // all present. Failure logs and continues without grounding.
    let emote_provider =
        build_emote_provider(config.ai.is_some(), &settings_handle, emote_tracker.clone());

    let twitch_channel_id = match resolve_twitch_channel_id(&config, &credentials).await {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::error!(
                ?error,
                "Failed to resolve Twitch channel id; emote set ingest disabled"
            );
            None
        }
    };

    // Shared latency cell, seeded from settings. The latency handler re-bases
    // it live when `twitch.expected_latency` is edited; the 1337 handler reads
    // it for timing adjustments.
    let latency_value = Arc::new(std::sync::atomic::AtomicU32::new(
        settings_handle.load().twitch.expected_latency,
    ));
    metrics::observe_irc_latency(Arc::clone(&latency_value));
    // One interpreter semaphore for `!bash` and Aurora's `run_bash`.
    let bash_permits = twitch_1337_core::commands::bash::shared_permits(&settings_handle);
    let services = Services {
        clock: Arc::new(SystemClock),
        llm,
        aviation: aviation_client,
        dhl: dhl_client,
        ipinfo: ipinfo_client,
        amp: amp_client,
        finnhub: finnhub_client,
        doener: doener_client,
        whisper: Some(whisper),
        data_dir: get_data_dir(),
        settings: settings_handle,
        settings_store,
        irc_connected,
        latency_value,
        channel: config.twitch.channel.clone(),
        bot_username: config.twitch.username.clone(),
        public_url: config.web.enabled.then(|| config.web.public_url.clone()),
        ai_enabled: config.ai.is_some(),
        started_at,
        ping: ping_handle,
        ping_names_rx,
        command_catalog: Default::default(),
        aurora_tools: Default::default(),
        memory_store,
        ai_memory,
        chat_ctx,
        emote_provider,
        leaderboard,
        hall_of_shame,
        aviation_tracker_tx,
        primary_history_tap: None,
        schedule_runtime,
        emote_tracker,
        emotes_agent: Default::default(),
        twitch_channel_id,
        mouths,
        echo,
        suspension,
        recap: recap_handle,
        bash_permits,
        volanta: Arc::new(twitch_1337_core::aviation::volanta::VolantaHub::load(
            &get_data_dir(),
        )),
        log_ring,
    };
    // `web_listener` is `Some` exactly when the web server is enabled.
    let web_spawner = match web_listener {
        Some(listener) => {
            Some(build_web_spawner(&config, credentials.clone(), &services, listener).await?)
        }
        None => None,
    };

    Ok(Prepared {
        client,
        incoming,
        services,
        ping_runtime,
        aviation_tracker_rx,
        web_spawner,
    })
}

/// Translate either process signal into the one intentional shutdown request
/// consumed by `run_bot`. Registration and delivery errors are faults; neither
/// Ctrl-C nor SIGTERM is silently ignored.
async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .wrap_err("register SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.wrap_err("wait for Ctrl-C")?;
            }
            signal = sigterm.recv() => {
                signal.ok_or_else(|| eyre!("SIGTERM signal stream closed"))?;
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.wrap_err("wait for Ctrl-C")?;
    }

    Ok(())
}

#[expect(
    clippy::map_err_ignore,
    reason = "oneshot SendError only returns the unsent unit value"
)]
fn request_shutdown(shutdown: oneshot::Sender<()>) -> Result<()> {
    shutdown
        .send(())
        .map_err(|_| eyre!("bot shutdown receiver was already dropped"))
}

fn join_bot_task(task: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    task.wrap_err("bot task panicked")?
}
/// Build the web-task spawn closure. Resolves the broadcaster id, builds
/// the helix client + OAuth context, and returns a closure that — given the
/// shared latched lifecycle — spawns the server and session-persistence
/// handles. The listener is bound in `run` (port-in-use aborts startup loudly
/// there) and passed in.
async fn build_web_spawner(
    config: &twitch_1337::config::Configuration,
    credentials: AuthenticatedLoginCredentials,
    services: &twitch_1337::Services,
    listener: tokio::net::TcpListener,
) -> Result<twitch_1337::WebSpawner> {
    let bind_addr = listener
        .local_addr()
        .wrap_err("query web listener local addr")?;

    let token_provider: Arc<dyn AccessTokenProvider> = Arc::new(CredsTokenProvider {
        creds: Arc::new(credentials),
    });

    let helix = Arc::new(ReqwestHelixClient::new(
        helix_http_client()?,
        config.twitch.client_id.clone(),
        token_provider,
    ));

    let broadcaster_id = match &services.twitch_channel_id {
        Some(id) => id.clone(),
        // `None` means the earlier resolve failed (emote ingest disabled);
        // the dashboard still needs a broadcaster id, so try once here.
        None => {
            helix
                .as_ref()
                .fetch_user_by_login(&config.twitch.channel)
                .await
                .wrap_err("resolve broadcaster id")?
                .ok_or_else(|| eyre!("channel `{}` not found on twitch", config.twitch.channel))?
                .id
        }
    };

    let oauth = Arc::new(twitch_1337_web::auth::OAuthCtx::new(
        config.twitch.client_id.expose_secret(),
        &config.twitch.client_secret,
        &config.web.public_url,
    )?);

    let web_clock = Arc::new(twitch_1337_web::clock::SystemClock);
    // Disk-backed so a rolling deploy (bot restart) does not log every
    // dashboard user out — the signed sid cookie keeps matching a live entry.
    let sessions = Arc::new(twitch_1337_web::auth::session::SessionTable::load(
        web_clock.clone(),
        get_data_dir().join("sessions.ron"),
    ));
    // Session persistence is owned by the web runtime's lifecycle task, not a
    // detached interval task in this composition function.

    let web_config = Arc::new(twitch_1337_web::config::WebConfig {
        bind_addr: config.web.bind_addr.clone(),
        public_url: config.web.public_url.clone(),
        session_secret: config.web.session_secret.clone(),
    });

    let signed_key = twitch_1337_web::state::derive_session_key(&config.web.session_secret)?;

    #[cfg(feature = "dev-login")]
    {
        tracing::warn!(
            target: "twitch_1337_web",
            "dev-login feature compiled in — /_dev/login mints mod sessions without OAuth (DO NOT SHIP)",
        );
    }

    let memory_audit = Arc::new(twitch_1337::settings::FileAuditLog::new(
        get_data_dir().join("memory_audit.log"),
    ));
    let state = twitch_1337_web::WebState::new(
        services,
        twitch_1337_web::state::WebBoot {
            sessions,
            helix: helix as Arc<dyn twitch_1337_web::helix::HelixClient>,
            config: web_config,
            clock: web_clock,
            broadcaster_id: Arc::from(broadcaster_id.as_str()),
            client_id: config.twitch.client_id.clone(),
            oauth,
            signed_key,
            avatar_cache: Arc::new(twitch_1337_web::helix::AvatarCache::new(
                std::time::Duration::from_secs(3600),
            )),
            owner: Arc::new(arc_swap::ArcSwap::from_pointee(config.twitch.owner.clone())),
            memory_audit,
        },
    );

    Ok(Box::new(move |lifecycle| {
        let deps = twitch_1337_web::WebDeps { bind_addr, state };
        twitch_1337_web::spawn_web_runtime(listener, deps, lifecycle)
    }))
}

/// Resolve the broadcaster's Twitch user id for 7TV ingest subscription.
async fn resolve_twitch_channel_id(
    config: &twitch_1337::config::Configuration,
    credentials: &AuthenticatedLoginCredentials,
) -> Result<String> {
    let token_provider: Arc<dyn AccessTokenProvider> = Arc::new(CredsTokenProvider {
        creds: Arc::new(credentials.clone()),
    });
    let helix = ReqwestHelixClient::new(
        helix_http_client()?,
        config.twitch.client_id.clone(),
        token_provider,
    );

    let broadcaster = helix
        .fetch_user_by_login(&config.twitch.channel)
        .await
        .wrap_err("resolve broadcaster id for emote ingest")?
        .ok_or_else(|| eyre!("channel `{}` not found on twitch", config.twitch.channel))?;

    Ok(broadcaster.id)
}

/// Bridges the bot's `RefreshingLoginCredentials` to the web crate's
/// helix `AccessTokenProvider`. Lets the helix client reuse the same
/// refreshed access token the bot already maintains in `token.ron`.
struct CredsTokenProvider {
    creds: Arc<AuthenticatedLoginCredentials>,
}

#[async_trait]
impl AccessTokenProvider for CredsTokenProvider {
    async fn current_access_token(&self) -> eyre::Result<String> {
        let creds = self
            .creds
            .as_ref()
            .get_credentials()
            .await
            .map_err(|e| eyre!("get_credentials: {e}"))?;
        Ok(creds.token.unwrap_or_default())
    }
}

/// Probe wildcard listeners through same-family loopback; preserve concrete bind addresses.
fn healthcheck_url(bind_addr: &str) -> Result<String> {
    let bind_addr: std::net::SocketAddr = bind_addr.parse().wrap_err("parse web.bind_addr")?;
    let ip = match bind_addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
        ip => ip,
    };
    let address = std::net::SocketAddr::new(ip, bind_addr.port());
    Ok(format!("http://{address}/healthz"))
}

/// Lightweight healthcheck for the Docker `HEALTHCHECK` directive. Reads the
/// config to find the web bind address and probes `/healthz`. When `[web]` is
/// disabled, exits 0 without touching the network so the container is still
/// considered healthy.
async fn run_healthcheck() -> Result<()> {
    let config = load_configuration().await?;
    if !config.web.enabled {
        return Ok(());
    }
    let url = healthcheck_url(&config.web.bind_addr)?;
    let res = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?
        .get(&url)
        .send()
        .await?;
    if res.status().is_success() {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_aviation_client, healthcheck_url, helix_http_client_with_timeout, request_shutdown,
    };
    use std::{sync::Arc, time::Duration};

    use secrecy::SecretString;
    use tokio::sync::oneshot;
    use twitch_1337_core::config::AviationstackBootstrap;
    use twitch_1337_core::settings::Settings;
    use twitch_1337_web::helix::{AccessTokenProvider, HelixClient as _, ReqwestHelixClient};

    struct StaticToken;

    #[async_trait::async_trait]
    impl AccessTokenProvider for StaticToken {
        async fn current_access_token(&self) -> eyre::Result<String> {
            Ok("test-token".to_owned())
        }
    }

    #[tokio::test]
    async fn one_shutdown_request_is_sent_once() {
        let (sender, receiver) = oneshot::channel();

        request_shutdown(sender).unwrap();
        assert_eq!(receiver.await.unwrap(), ());
    }

    #[test]
    fn dropped_shutdown_receiver_is_a_signal_adapter_fault() {
        let (sender, receiver) = oneshot::channel::<()>();
        drop(receiver);

        let error = request_shutdown(sender).expect_err("a dropped receiver must be reported");
        assert!(error.to_string().contains("receiver was already dropped"));
    }

    #[test]
    fn disabled_aviationstack_still_builds_adsb_client() {
        twitch_1337_core::install_crypto_provider();
        let settings = Settings::compiled_defaults().aviationstack;

        let client = build_aviation_client(None, &settings).expect("aviation client");

        assert!(!client.aviationstack_enabled());
    }

    #[test]
    fn enabled_aviationstack_attaches_metadata_config() {
        twitch_1337_core::install_crypto_provider();
        let mut settings = Settings::compiled_defaults().aviationstack;
        settings.enabled = true;
        let bootstrap = AviationstackBootstrap {
            api_key: SecretString::new("test-key".to_owned().into()),
        };

        let client = build_aviation_client(Some(&bootstrap), &settings).expect("aviation client");

        assert!(client.aviationstack_enabled());
    }

    #[tokio::test]
    async fn stalled_helix_response_hits_the_client_timeout() {
        twitch_1337_core::install_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stalled Helix server");
        let address = listener.local_addr().expect("stalled server address");
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept Helix request");
            std::future::pending::<()>().await;
        });
        let helix = ReqwestHelixClient::with_base(
            helix_http_client_with_timeout(Duration::from_millis(50)).expect("Helix HTTP client"),
            SecretString::new("test-client-id".to_owned().into()),
            Arc::new(StaticToken),
            format!("http://{address}"),
        );

        let response = tokio::time::timeout(
            Duration::from_secs(2),
            helix.fetch_user_by_login("test-user"),
        )
        .await;

        assert!(
            response.is_ok(),
            "request escaped the Helix client's own deadline"
        );
        assert!(response.expect("client deadline").is_err());
        server.abort();
    }

    #[test]
    fn healthcheck_targets_the_configured_bind_address() {
        for (bind_addr, expected) in [
            ("127.0.0.1:8080", "http://127.0.0.1:8080/healthz"),
            ("0.0.0.0:8080", "http://127.0.0.1:8080/healthz"),
            ("192.0.2.10:8080", "http://192.0.2.10:8080/healthz"),
            ("[::1]:8080", "http://[::1]:8080/healthz"),
            ("[::]:8080", "http://[::1]:8080/healthz"),
        ] {
            assert_eq!(healthcheck_url(bind_addr).unwrap(), expected);
        }
    }
}
