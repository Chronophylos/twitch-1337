//! Configuration types loaded from config.toml.
//!
//! These are kept in the library so that handler modules (and integration
//! tests) can reference them without going through the binary entry point.

use std::collections::HashMap;

use eyre::{Result, WrapErr, bail};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use tracing::info;

/// Bootstrap-only Twitch configuration. Only the fields required to connect
/// to Twitch IRC and authenticate belong here. Runtime knobs (expected_latency,
/// hidden_admins, viewer_allowlist, admin_channel, ai_channel) live in
/// the dashboard settings store.
#[derive(Debug, Clone, Deserialize)]
pub struct TwitchConfiguration {
    pub channel: String,
    pub username: String,
    pub refresh_token: SecretString,
    pub client_id: SecretString,
    pub client_secret: SecretString,
    /// Twitch user ID with full dashboard access including the settings page.
    /// Single value for v1; a tiered permission system replaces it later.
    /// Absent → no owner exists and the settings page returns 403.
    #[serde(default)]
    pub owner: Option<String>,
    /// Bootstrap credentials for secondary AI mouth accounts. Absent →
    /// that mouth is disabled.
    #[serde(default)]
    pub mouths: Mouths,
}

/// Bootstrap-only credentials for a secondary AI mouth account.
#[derive(Debug, Clone, Deserialize)]
pub struct MouthConfig {
    pub username: String,
    pub refresh_token: SecretString,
}

/// Bootstrap-only container for optional secondary AI mouth accounts.
/// Each field is absent unless a `[twitch.mouths.<name>]` section is
/// present in config.toml.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Mouths {
    #[serde(default)]
    pub aurora: Option<MouthConfig>,
    #[serde(default)]
    pub grok: Option<MouthConfig>,
}

/// Bootstrap-only aviationstack configuration. Only the secret api_key belongs
/// here; `enabled`, `base_url`, and `timeout_secs` live in the dashboard
/// settings store under `aviationstack.*`.
#[derive(Debug, Clone, Deserialize)]
pub struct AviationstackBootstrap {
    pub api_key: SecretString,
}

/// Bootstrap-only DHL configuration. The api_key is the whole surface:
/// its presence is the enable switch. Unlike aviationstack there is no
/// settings-store section — AI tools have no per-tool dashboard toggles.
#[derive(Debug, Clone, Deserialize)]
pub struct DhlBootstrap {
    pub api_key: SecretString,
}

/// Bootstrap-only AI configuration. Secrets only — every other AI knob
/// lives in the dashboard settings store and is read from the
/// SettingsHandle at runtime.
///
/// Deserialize-only on purpose: a `Serialize` derive here would let a debug
/// dump round-trip the API keys out of the process.
#[derive(Debug, Clone, Deserialize)]
pub struct AiBootstrap {
    /// Leftover from before named keys. Ignored at runtime — operators
    /// should move the secret into `[ai.keys]` and point each profile at a
    /// key name. Kept so we can warn instead of silently dropping it.
    #[serde(default)]
    pub api_key: Option<SecretString>,
    /// `[ai.keys]` — named credentials a profile points at by name, so a
    /// profile in settings.ron holds a name and never a secret.
    #[serde(default)]
    pub keys: HashMap<String, SecretString>,
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
}

fn default_web_bind() -> String {
    "127.0.0.1:8080".to_owned()
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
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Configuration {
    pub twitch: TwitchConfiguration,
    #[serde(default)]
    pub aviationstack: Option<AviationstackBootstrap>,
    #[serde(default)]
    pub dhl: Option<DhlBootstrap>,
    #[serde(default)]
    pub ai: Option<AiBootstrap>,
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
                owner: None,
                mouths: Mouths::default(),
            },
            aviationstack: None,
            dhl: None,
            ai: None,
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

    if config
        .aviationstack
        .as_ref()
        .is_some_and(|av| av.api_key.expose_secret().trim().is_empty())
    {
        bail!("aviationstack.api_key cannot be empty when aviationstack is configured");
    }

    if config
        .dhl
        .as_ref()
        .is_some_and(|d| d.api_key.expose_secret().trim().is_empty())
    {
        bail!("dhl.api_key cannot be empty when dhl is configured");
    }

    if let Some(ref ai) = config.ai
        && ai
            .keys
            .values()
            .any(|k| k.expose_secret().trim().is_empty())
    {
        bail!("[ai.keys] entries cannot be empty");
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
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_bootstrap_parses_named_keys() {
        let cfg: Configuration = toml::from_str(
            r#"
            [twitch]
            channel = "c"
            username = "u"
            refresh_token = "r"
            client_id = "i"
            client_secret = "s"

            [ai.keys]
            openrouter = "sk-test"
        "#,
        )
        .expect("parse");
        let boot = cfg.ai.as_ref().expect("ai present");
        assert_eq!(
            boot.keys
                .get("openrouter")
                .expect("named key")
                .expose_secret(),
            "sk-test"
        );
        assert!(boot.api_key.is_none());
    }

    #[test]
    fn leftover_ai_api_key_still_parses_but_is_unused() {
        let cfg: Configuration = toml::from_str(
            r#"
            [twitch]
            channel = "c"
            username = "u"
            refresh_token = "r"
            client_id = "i"
            client_secret = "s"

            [ai]
            api_key = "sk-legacy"
        "#,
        )
        .expect("parse");
        let boot = cfg.ai.as_ref().expect("ai present");
        assert_eq!(
            boot.api_key.as_ref().expect("leftover key").expose_secret(),
            "sk-legacy"
        );
        assert!(boot.keys.is_empty());
    }

    #[test]
    fn empty_named_key_is_rejected() {
        let mut cfg = Configuration::test_default();
        cfg.ai = Some(AiBootstrap {
            api_key: None,
            keys: std::collections::HashMap::from([(
                "openrouter".into(),
                SecretString::new("   ".into()),
            )]),
        });
        let err = validate_config(&cfg).expect_err("empty named key must fail");
        assert!(
            err.to_string().contains("[ai.keys]"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn aviationstack_bootstrap_parses_api_key_only() {
        let cfg: Configuration = toml::from_str(
            r#"
            [twitch]
            channel = "c"
            username = "u"
            refresh_token = "r"
            client_id = "i"
            client_secret = "s"

            [aviationstack]
            api_key = "av-key"
        "#,
        )
        .expect("parse");
        let boot = cfg.aviationstack.as_ref().expect("aviationstack present");
        assert!(!boot.api_key.expose_secret().is_empty());
    }

    #[test]
    fn legacy_twitch_keys_silently_ignored() {
        // Fields that have moved to the settings store should parse without error.
        // `owner` is back in TwitchConfiguration, so it is parsed normally.
        let cfg: Configuration = toml::from_str(
            r#"
            [twitch]
            channel = "c"
            username = "u"
            refresh_token = "r"
            client_id = "i"
            client_secret = "s"
            expected_latency = 200
            hidden_admins = ["123"]
            owner = "456"
            admin_channel = "admins"
            ai_channel = "ai"
        "#,
        )
        .expect("parse — legacy keys are silently ignored by serde (owner is parsed)");
        assert_eq!(cfg.twitch.channel, "c");
        assert_eq!(cfg.twitch.owner.as_deref(), Some("456"));
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
    fn parses_optional_mouths() {
        let toml = r#"
channel = "c"
username = "u"
refresh_token = "r"
client_id = "ci"
client_secret = "cs"

[mouths.aurora]
username = "aurora_bot"
refresh_token = "ar"
"#;
        let cfg: super::TwitchConfiguration = toml::from_str(toml).unwrap();
        assert_eq!(cfg.mouths.aurora.as_ref().unwrap().username, "aurora_bot");
        assert!(cfg.mouths.grok.is_none());
    }

    #[test]
    fn absent_mouths_default_empty() {
        let toml = r#"
channel = "c"
username = "u"
refresh_token = "r"
client_id = "ci"
client_secret = "cs"
"#;
        let cfg: super::TwitchConfiguration = toml::from_str(toml).unwrap();
        assert!(cfg.mouths.aurora.is_none() && cfg.mouths.grok.is_none());
    }

    #[test]
    fn dhl_section_parses_api_key() {
        let toml = r#"
[twitch]
channel = "chan"
username = "bot"
refresh_token = "r"
client_id = "c"
client_secret = "s"

[dhl]
api_key = "abc123"
"#;
        let cfg: Configuration = toml::from_str(toml).expect("parse");
        let dhl = cfg.dhl.expect("dhl section present");
        assert_eq!(dhl.api_key.expose_secret(), "abc123");
    }

    #[test]
    fn dhl_section_is_optional() {
        let toml = r#"
[twitch]
channel = "chan"
username = "bot"
refresh_token = "r"
client_id = "c"
client_secret = "s"
"#;
        let cfg: Configuration = toml::from_str(toml).expect("parse");
        assert!(cfg.dhl.is_none());
    }

    #[test]
    fn empty_dhl_api_key_is_rejected() {
        let mut cfg = Configuration::test_default();
        cfg.dhl = Some(DhlBootstrap {
            api_key: SecretString::new("   ".into()),
        });
        let err = validate_config(&cfg).expect_err("empty key must fail");
        assert!(
            err.to_string().contains("dhl.api_key"),
            "unexpected error: {err}"
        );
    }
}
