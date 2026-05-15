//! Configuration types loaded from config.toml.
//!
//! These are kept in the library so that handler modules (and integration
//! tests) can reference them without going through the binary entry point.

use eyre::{Result, WrapErr, bail};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use tracing::info;

use crate::database;

fn default_expected_latency() -> u32 {
    100
}

#[derive(Debug, Clone, Deserialize)]
pub struct TwitchConfiguration {
    pub channel: String,
    pub username: String,
    pub refresh_token: SecretString,
    pub client_id: SecretString,
    pub client_secret: SecretString,
    #[serde(default = "default_expected_latency")]
    pub expected_latency: u32,
    #[serde(default)]
    pub hidden_admins: Vec<String>,
    /// Twitch user IDs granted read-only viewer access to the web dashboard.
    /// IDs (not logins) so entries survive Twitch login renames.
    #[serde(default)]
    pub viewer_allowlist: Vec<String>,
    /// Twitch user ID granted full dashboard access including the settings
    /// page. Single value for v1; a tiered permission system replaces it
    /// later. Absent → no owner exists and the settings page returns 403.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub admin_channel: Option<String>,
    #[serde(default)]
    pub ai_channel: Option<String>,
}

fn default_aviationstack_base_url() -> String {
    "https://api.aviationstack.com/v1".to_string()
}

fn default_aviationstack_timeout_secs() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
pub struct AviationstackConfig {
    #[serde(default)]
    pub enabled: bool,
    pub api_key: SecretString,
    #[serde(default = "default_aviationstack_base_url")]
    pub base_url: String,
    #[serde(default = "default_aviationstack_timeout_secs")]
    pub timeout_secs: u64,
}

/// Bootstrap-only AI configuration. The secret api_key stays in
/// config.toml; every other knob lives in the dashboard settings
/// store and is read from the SettingsHandle at runtime.
#[derive(Debug, Clone, Deserialize)]
pub struct AiBootstrap {
    pub api_key: SecretString,
}

fn default_suspend_duration() -> u64 {
    600
}

#[derive(Debug, Clone, Deserialize)]
pub struct SuspendConfig {
    #[serde(default = "default_suspend_duration")]
    pub default_duration_secs: u64,
}

impl Default for SuspendConfig {
    fn default() -> Self {
        Self {
            default_duration_secs: default_suspend_duration(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

/// Configuration for a scheduled message loaded from config.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleConfig {
    pub name: String,
    pub message: String,
    /// Interval in "hh:mm" format (e.g., "01:30" for 1 hour 30 minutes)
    pub interval: String,
    /// Start date in ISO 8601 format (YYYY-MM-DDTHH:MM:SS)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date in ISO 8601 format (YYYY-MM-DDTHH:MM:SS)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Daily active time start in HH:MM format
    #[serde(default)]
    pub active_time_start: Option<String>,
    /// Daily active time end in HH:MM format
    #[serde(default)]
    pub active_time_end: Option<String>,
    /// Whether the schedule is enabled (default: true)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_web_bind")]
    pub bind_addr: String,
    #[serde(default)]
    pub public_url: String,
    #[serde(default = "default_web_session_secret")]
    pub session_secret: SecretString,
    #[serde(default = "default_session_ttl", with = "humantime_serde")]
    pub session_ttl: std::time::Duration,
    #[serde(default = "default_mod_check_refresh", with = "humantime_serde")]
    pub mod_check_refresh: std::time::Duration,
}

fn default_web_bind() -> String {
    "127.0.0.1:8080".to_owned()
}
fn default_session_ttl() -> std::time::Duration {
    std::time::Duration::from_secs(7 * 24 * 60 * 60)
}
fn default_mod_check_refresh() -> std::time::Duration {
    std::time::Duration::from_secs(300)
}
fn default_web_session_secret() -> SecretString {
    SecretString::new(String::new().into())
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: default_web_bind(),
            public_url: String::new(),
            session_secret: default_web_session_secret(),
            session_ttl: default_session_ttl(),
            mod_check_refresh: default_mod_check_refresh(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Configuration {
    pub twitch: TwitchConfiguration,
    #[serde(default)]
    pub aviationstack: Option<AviationstackConfig>,
    #[serde(default)]
    pub suspend: SuspendConfig,
    #[serde(default)]
    pub ai: Option<AiBootstrap>,
    #[serde(default)]
    pub schedules: Vec<ScheduleConfig>,
    #[serde(default)]
    pub web: WebConfig,
}

#[cfg(any(test, feature = "testing"))]
impl Configuration {
    /// Minimal configuration suitable for integration tests. Channel =
    /// "test_chan", username = "bot", no AI, no schedules, default ping
    /// cooldown. Tests override fields via `TestBotBuilder::with_config`.
    pub fn test_default() -> Self {
        Self {
            twitch: TwitchConfiguration {
                channel: "test_chan".to_owned(),
                username: "bot".to_owned(),
                refresh_token: SecretString::new("test".into()),
                client_id: SecretString::new("test".into()),
                client_secret: SecretString::new("test".into()),
                expected_latency: 100,
                hidden_admins: Vec::new(),
                viewer_allowlist: Vec::new(),
                owner: None,
                admin_channel: None,
                ai_channel: None,
            },
            aviationstack: None,
            suspend: SuspendConfig::default(),
            ai: None,
            schedules: Vec::new(),
            web: WebConfig::default(),
        }
    }
}

/// Load and validate configuration from the standard config path.
///
/// Returns both the deserialized `Configuration` and the raw `toml::Value` so
/// callers can inspect legacy keys (e.g. the one-shot migration helper in
/// `main.rs`) without re-parsing the file.
pub async fn load_configuration() -> Result<(Configuration, toml::Value)> {
    let config_path = crate::get_config_path();
    let data = tokio::fs::read_to_string(&config_path)
        .await
        .wrap_err_with(|| {
            format!(
                "Failed to read config file: {}\nPlease create config.toml from config.toml.example",
                config_path.display()
            )
        })?;

    info!("Loading configuration from {}", config_path.display());

    let value: toml::Value =
        toml::from_str(&data).wrap_err("Failed to parse config.toml - check for syntax errors")?;

    let config: Configuration = value
        .clone()
        .try_into()
        .wrap_err("Failed to deserialize config.toml into Configuration")?;

    validate_config(&config)?;

    info!(
        owner_configured = config.twitch.owner.is_some(),
        "Resolved dashboard owner"
    );

    Ok((config, value))
}

/// Validate config fields beyond what serde can express.
pub fn validate_config(config: &Configuration) -> Result<()> {
    if config.twitch.channel.trim().is_empty() {
        bail!("twitch.channel cannot be empty");
    }

    if config.twitch.username.trim().is_empty() {
        bail!("twitch.username cannot be empty");
    }

    if config.twitch.expected_latency > 1000 {
        bail!(
            "twitch.expected_latency must be <= 1000ms (got {})",
            config.twitch.expected_latency
        );
    }

    if let Some(ref admin_ch) = config.twitch.admin_channel {
        if admin_ch.trim().is_empty() {
            bail!("twitch.admin_channel cannot be empty when specified");
        }
        if admin_ch == &config.twitch.channel {
            bail!("twitch.admin_channel must be different from twitch.channel");
        }
    }

    if let Some(ref ai_ch) = config.twitch.ai_channel {
        if ai_ch.trim().is_empty() {
            bail!("twitch.ai_channel cannot be empty when specified");
        }
        if ai_ch == &config.twitch.channel {
            bail!("twitch.ai_channel must be different from twitch.channel");
        }
        // Cross-check: admin_channel block above cannot see ai_channel, so the
        // admin_channel == ai_channel guard lives here. Keep this branch second.
        if let Some(ref admin_ch) = config.twitch.admin_channel
            && ai_ch == admin_ch
        {
            bail!("twitch.ai_channel must be different from twitch.admin_channel");
        }
    }

    if !(1..=7 * 86400).contains(&config.suspend.default_duration_secs) {
        bail!(
            "suspend.default_duration_secs must be between 1 and 604800 (7 days) (got {})",
            config.suspend.default_duration_secs
        );
    }

    if let Some(ref aviationstack) = config.aviationstack {
        if aviationstack.enabled && aviationstack.api_key.expose_secret().trim().is_empty() {
            bail!("aviationstack.api_key cannot be empty when aviationstack is enabled");
        }
        if aviationstack.base_url.trim().is_empty() {
            bail!("aviationstack.base_url cannot be empty");
        }
        reqwest::Url::parse(&aviationstack.base_url).wrap_err_with(|| {
            format!(
                "aviationstack.base_url must be a valid URL (got {:?})",
                aviationstack.base_url
            )
        })?;
        if aviationstack.timeout_secs == 0 {
            bail!("aviationstack.timeout_secs must be > 0");
        }
    }

    if let Some(ref ai) = config.ai
        && ai.api_key.expose_secret().trim().is_empty()
    {
        bail!("ai.api_key cannot be empty");
    }

    for schedule in &config.schedules {
        if schedule.name.trim().is_empty() {
            bail!("Schedule name cannot be empty");
        }
        if schedule.message.trim().is_empty() {
            bail!("Schedule '{}' message cannot be empty", schedule.name);
        }
        if schedule.interval.trim().is_empty() {
            bail!("Schedule '{}' interval cannot be empty", schedule.name);
        }
        database::Schedule::parse_interval(&schedule.interval).wrap_err_with(|| {
            format!("Schedule '{}' has invalid interval format", schedule.name)
        })?;
    }

    if config.web.enabled {
        let secret = config.web.session_secret.expose_secret();
        let secret_bytes_len = hex::decode(secret).map(|b| b.len()).unwrap_or(0);
        if secret_bytes_len < 32 {
            bail!("web.session_secret must be ≥32 bytes hex when web.enabled = true");
        }
        if !config.web.public_url.starts_with("https://") {
            bail!(
                "web.public_url must start with https:// when web.enabled = true (got {:?})",
                config.web.public_url
            );
        }
        let ttl = config.web.session_ttl.as_secs();
        if !(3600..=2_592_000).contains(&ttl) {
            bail!("web.session_ttl must be between 1h and 30d (got {ttl}s)");
        }
        let refresh = config.web.mod_check_refresh.as_secs();
        if !(30..=3600).contains(&refresh) {
            bail!("web.mod_check_refresh must be between 30s and 1h (got {refresh}s)");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_channel_must_differ_from_main_channel() {
        let mut config = Configuration::test_default();
        config.twitch.ai_channel = Some(config.twitch.channel.clone());
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(
            err.contains("ai_channel must be different from twitch.channel"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ai_channel_must_differ_from_admin_channel() {
        let mut config = Configuration::test_default();
        config.twitch.admin_channel = Some("admins".into());
        config.twitch.ai_channel = Some("admins".into());
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(
            err.contains("ai_channel must be different from twitch.admin_channel"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ai_channel_cannot_be_blank_when_set() {
        let mut config = Configuration::test_default();
        config.twitch.ai_channel = Some("   ".into());
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(
            err.contains("ai_channel cannot be empty when specified"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ai_channel_some_distinct_value_validates() {
        let mut config = Configuration::test_default();
        config.twitch.ai_channel = Some("ai_chan".into());
        validate_config(&config).expect("distinct ai_channel must validate");
    }

    #[test]
    fn ai_bootstrap_parses_api_key_only() {
        let cfg: Configuration = toml::from_str(
            r#"
            [twitch]
            channel = "c"
            username = "u"
            refresh_token = "r"
            client_id = "i"
            client_secret = "s"

            [ai]
            api_key = "sk-test"
        "#,
        )
        .expect("parse");
        let boot = cfg.ai.as_ref().expect("ai present");
        assert!(!boot.api_key.expose_secret().is_empty());
    }

    #[test]
    fn web_disabled_skips_validation() {
        let cfg = Configuration::test_default();
        assert!(!cfg.web.enabled);
        validate_config(&cfg).expect("disabled web validates trivially");
    }

    #[test]
    fn web_enabled_requires_https_public_url() {
        let mut cfg = Configuration::test_default();
        cfg.web.enabled = true;
        cfg.web.session_secret = secrecy::SecretString::new("00".repeat(32).into());
        cfg.web.public_url = "http://insecure".into();
        let err = validate_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("public_url"), "{err}");
    }

    #[test]
    fn web_enabled_requires_32_byte_secret() {
        let mut cfg = Configuration::test_default();
        cfg.web.enabled = true;
        cfg.web.session_secret = secrecy::SecretString::new("ab".into());
        cfg.web.public_url = "https://bot.test".into();
        let err = validate_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("session_secret"), "{err}");
    }

    #[test]
    fn web_enabled_validates_ttl_range() {
        let mut cfg = Configuration::test_default();
        cfg.web.enabled = true;
        cfg.web.session_secret = secrecy::SecretString::new("00".repeat(32).into());
        cfg.web.public_url = "https://bot.test".into();
        cfg.web.session_ttl = std::time::Duration::from_secs(10); // below 1h
        let err = validate_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("session_ttl"), "{err}");
    }
}
