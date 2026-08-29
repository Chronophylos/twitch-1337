use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use chrono::Utc;
use color_eyre::eyre::Result;
use eyre::{WrapErr as _, eyre};
use secrecy::ExposeSecret as _;
use tokio::sync::oneshot;
use tracing::info;
use twitch_1337_core::{
    AuthenticatedLoginCredentials, PersonalBest, PersonalWorst, Services,
    ai::command::ChatContext,
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
    install_tracing();
    install_crypto_provider();
    let config = load_configuration().await?;

    ensure_data_dir().await?;

    // Dashboard-managed runtime settings. Opened before IRC connect so the
    // settings-store values (admin_channel, ai_channel) can be passed to the
    // IRC setup which needs them to join channels at startup.
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
    let local = Utc::now().with_timezone(&chrono_tz::Europe::Berlin);
    let initial_schedule_count = settings_handle.load().schedules.len();
    info!(
        local_time = ?local,
        utc_time = ?Utc::now(),
        channel = %config.twitch.channel,
        username = %config.twitch.username,
        schedule_count = initial_schedule_count,
        "Starting twitch-1337 bot"
    );

    // Collect restart-required channel list for IRC channel joins.
    let twitch_runtime = settings_handle.load().twitch.clone();
    let extra_channels: Vec<String> = [
        twitch_runtime.admin_channel.clone(),
        twitch_runtime.ai_channel.clone(),
    ]
    .into_iter()
    .flatten()
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

    let langfuse = match config.langfuse.as_ref() {
        Some(b) => match twitch_1337_core::ai::langfuse::LangfuseHandle::start(b) {
            Ok(h) => {
                tracing::info!("Langfuse OTLP export enabled");
                Some(Arc::new(h))
            }
            Err(error) => {
                tracing::error!(
                    ?error,
                    "failed to start Langfuse exporter; tracing disabled"
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

    // Build the v2 memory bundle (store + transcript + knobs) once at
    // composition; `run_bot` reads it from `Services`. `None` when AI is off.
    let ai_memory = twitch_1337::ai::command::build_ai_memory_v2(
        config.ai.is_some(),
        &settings_handle.load_full(),
        memory_store.clone(),
    )
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

    // Build telemetry once so the IRC orchestrator and WebState share the same Arc.
    let telemetry = twitch_1337::schedule::TelemetryStore::open(&get_data_dir());

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

    let services = Services {
        clock: Arc::new(SystemClock),
        llm,
        aviation: aviation_client,
        dhl: dhl_client,
        langfuse,
        doener: doener_client,
        whisper: Some(whisper),
        data_dir: get_data_dir(),
        settings: settings_handle,
        settings_store,
        irc_connected,
        channel: config.twitch.channel.clone(),
        bot_username: config.twitch.username.clone(),
        ai_enabled: config.ai.is_some(),
        started_at,
        ping: ping_handle,
        ping_names_rx,
        memory_store,
        ai_memory,
        chat_ctx,
        emote_provider,
        leaderboard,
        hall_of_shame,
        aviation_tracker_tx,
        primary_history_tap: None,
        telemetry,
        emote_tracker,
        twitch_channel_id,
        mouths,
        echo,
        suspension,
        recap: recap_handle,
    };
    let web_spawner = if config.web.enabled {
        Some(
            build_web_spawner(
                &config,
                credentials.clone(),
                &services,
                web_listener.expect("web enabled implies listener"),
            )
            .await?,
        )
    } else {
        None
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let _ = shutdown_tx.send(());
    });

    run_bot(
        client,
        incoming,
        services,
        ping_runtime,
        aviation_tracker_rx,
        web_spawner,
        shutdown_rx,
    )
    .await
}
/// Build the web-task spawn closure. Resolves the broadcaster id, builds
/// the helix client + OAuth context, and returns a closure that — given the
/// shared shutdown `Notify` — spawns `run_web` on a tokio task. The listener
/// is bound in `run` (port-in-use aborts startup loudly there) and passed in.
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
        reqwest::Client::new(),
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
    // Periodic snapshot so the sliding `last_seen` survives a restart too,
    // not just the login event. Login/logout persist immediately; this catches
    // the in-between activity. 5 min is well under the default 2 h session TTL.
    {
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                sessions.persist().await;
            }
        });
    }

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

    Ok(Box::new(move |shutdown| {
        let deps = twitch_1337_web::WebDeps { bind_addr, state };
        tokio::spawn(async move {
            if let Err(e) = twitch_1337_web::run_web(listener, deps, shutdown).await {
                tracing::error!(target: "twitch_1337_web", error = ?e, "Web task exited with error");
            }
        })
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
        reqwest::Client::new(),
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

/// Lightweight healthcheck for the Docker `HEALTHCHECK` directive. Reads the
/// config to find the web bind port and probes `/healthz`. When `[web]` is
/// disabled, exits 0 without touching the network so the container is still
/// considered healthy.
async fn run_healthcheck() -> Result<()> {
    let config = load_configuration().await?;
    if !config.web.enabled {
        return Ok(());
    }
    let port = config.web.bind_addr.rsplit(':').next().unwrap_or("8080");
    let url = format!("http://127.0.0.1:{port}/healthz");
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
    use super::build_aviation_client;
    use secrecy::SecretString;
    use twitch_1337_core::config::AviationstackBootstrap;
    use twitch_1337_core::settings::Settings;

    #[test]
    fn test_prefill_threshold_validation() {
        assert!((0.0..=1.0).contains(&0.0));
        assert!((0.0..=1.0).contains(&0.5));
        assert!((0.0..=1.0).contains(&1.0));
        assert!(!(0.0..=1.0).contains(&-0.1));
        assert!(!(0.0..=1.0).contains(&1.1));
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
}
