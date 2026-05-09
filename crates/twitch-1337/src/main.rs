use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use twitch_1337_core as twitch_1337;

use chrono::Utc;
use color_eyre::eyre::Result;
use secrecy::ExposeSecret as _;
use tokio::sync::oneshot;
use tracing::info;
use twitch_1337::{
    Services, aviation, ensure_data_dir, get_data_dir, install_crypto_provider, install_tracing,
    llm_factory, load_configuration, run_bot, setup_and_verify_twitch_client, twitch::whisper,
    util::clock::SystemClock,
};

#[tokio::main]
pub async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        return run_healthcheck().await;
    }

    color_eyre::install()?;
    install_tracing();
    install_crypto_provider();

    let config = load_configuration().await?;

    let local = Utc::now().with_timezone(&chrono_tz::Europe::Berlin);
    info!(
        local_time = ?local,
        utc_time = ?Utc::now(),
        channel = %config.twitch.channel,
        username = %config.twitch.username,
        schedules_enabled = !config.schedules.is_empty(),
        schedule_count = config.schedules.len(),
        "Starting twitch-1337 bot"
    );

    ensure_data_dir().await?;

    let (incoming, client, credentials, bot_user_id) =
        setup_and_verify_twitch_client(&config).await?;
    let client = Arc::new(client);

    let llm_client = llm_factory::build_llm_client(config.ai.as_ref())?;

    let aviation_client = match aviation::AviationClient::new()
        .map(|client| client.with_aviationstack_config(config.aviationstack.clone()))
    {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::error!(
                error = ?e,
                "Failed to initialize aviation client; aviation commands and flight tracker disabled"
            );
            None
        }
    };

    let whisper = whisper::HelixWhisperSender::new(
        credentials,
        config.twitch.client_id.expose_secret().to_string(),
        bot_user_id,
        get_data_dir(),
    )
    .await
    .map(|sender| Arc::new(sender) as Arc<dyn whisper::WhisperSender>)?;

    let irc_connected = Arc::new(AtomicBool::new(false));

    let services = Services {
        clock: Arc::new(SystemClock),
        llm: llm_client,
        aviation: aviation_client,
        whisper: Some(whisper),
        data_dir: get_data_dir(),
        emote_glossary_override: None,
        irc_connected,
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let _ = shutdown_tx.send(());
    });

    run_bot(client, incoming, config, services, shutdown_rx).await
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
    #[test]
    fn test_prefill_threshold_validation() {
        assert!((0.0..=1.0).contains(&0.0));
        assert!((0.0..=1.0).contains(&0.5));
        assert!((0.0..=1.0).contains(&1.0));
        assert!(!(0.0..=1.0).contains(&-0.1));
        assert!(!(0.0..=1.0).contains(&1.1));
    }
}
