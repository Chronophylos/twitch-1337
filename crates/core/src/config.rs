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
/// hidden_admins, viewer_allowlist, admin_channel) live in
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

/// Bootstrap-only ipinfo.io configuration. The token is the whole surface:
/// its presence adds geo, ASN and hostname to Aurora's `lookup_network` IP
/// results. Absent, the tool still runs on keyless RDAP.
#[derive(Debug, Clone, Deserialize)]
pub struct IpinfoBootstrap {
    pub token: SecretString,
}

/// Bootstrap-only AMP (CubeCoders game-server panel) configuration. Presence
/// enables `!servers`. Use a dedicated AMP user whose role only allows
/// `Instances.*`; the bot never starts or stops anything.
#[derive(Debug, Clone, Deserialize)]
pub struct AmpBootstrap {
    /// Controller origin, e.g. `http://192.0.2.1:8080`.
    pub url: String,
    pub username: String,
    pub password: SecretString,
}

/// Bootstrap-only Finnhub configuration. Presence registers `!aktien`;
/// the free-tier key is the whole surface.
#[derive(Debug, Clone, Deserialize)]
pub struct FinnhubBootstrap {
    pub api_key: SecretString,
}

/// Bootstrap-only GitHub App configuration. Presence lets the `/feedback`
/// dashboard turn chat feedback into issues and poll for closes. Runtime
/// knobs live in `settings.ron` under `feedback.*`.
///
/// Deserialize-only on purpose: the private key never round-trips out.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubBootstrap {
    /// Target repository, `owner/name`.
    pub repo: String,
    /// Numeric GitHub App id (the JWT `iss`).
    pub app_id: u64,
    /// Inline PEM: PKCS#1 (`BEGIN RSA PRIVATE KEY`, what GitHub hands out)
    /// or PKCS#8 (`BEGIN PRIVATE KEY`).
    pub private_key: SecretString,
}

/// Bootstrap-only Langfuse configuration. Presence of the section enables
/// first-party OTLP export. Keys never belong in settings.ron.
#[derive(Debug, Clone, Deserialize)]
pub struct LangfuseBootstrap {
    pub public_key: SecretString,
    pub secret_key: SecretString,
    /// Origin only, e.g. `https://cloud.langfuse.com`. Default is the EU cloud.
    #[serde(default)]
    pub host: Option<String>,
}

/// Bootstrap-only OTLP export configuration (ADR-0013). Presence enables
/// export of logs, traces and metrics to an OTLP/HTTP collector; the per-sink
/// filters live in `settings.ron` under `observability.otel`.
#[derive(Debug, Clone, Deserialize)]
pub struct OtelBootstrap {
    /// Collector base URL, e.g. `http://alloy:4318`. The exporters append
    /// `/v1/traces`, `/v1/logs` and `/v1/metrics`.
    pub endpoint: String,
    /// Sent verbatim as the `Authorization` header (`Basic …`, `Bearer …`).
    #[serde(default)]
    pub auth_header: Option<SecretString>,
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
    pub ipinfo: Option<IpinfoBootstrap>,
    #[serde(default)]
    pub amp: Option<AmpBootstrap>,
    #[serde(default)]
    pub finnhub: Option<FinnhubBootstrap>,
    #[serde(default)]
    pub github: Option<GithubBootstrap>,
    #[serde(default)]
    pub langfuse: Option<LangfuseBootstrap>,
    #[serde(default)]
    pub otel: Option<OtelBootstrap>,
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
            ipinfo: None,
            amp: None,
            finnhub: None,
            github: None,
            langfuse: None,
            otel: None,
            ai: None,
            web: WebConfig::default(),
        }
    }
}

/// Load and validate configuration from the standard config path.
pub async fn load_configuration() -> Result<Configuration> {
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

    let config: Configuration =
        toml::from_str(&data).wrap_err("Failed to deserialize config.toml into Configuration")?;

    validate_config(&config)?;

    Ok(config)
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

    if config
        .ipinfo
        .as_ref()
        .is_some_and(|i| i.token.expose_secret().trim().is_empty())
    {
        bail!("ipinfo.token cannot be empty when ipinfo is configured");
    }

    if let Some(amp) = config.amp.as_ref() {
        if amp.url.trim().is_empty() {
            bail!("amp.url cannot be empty when amp is configured");
        }
        // The client concatenates the raw string, so it must already be a
        // bare origin: no surrounding whitespace, query or fragment.
        let scheme_ok = amp.url.trim() == amp.url
            && reqwest::Url::parse(&amp.url).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https")
                    && u.has_host()
                    && u.query().is_none()
                    && u.fragment().is_none()
            });
        if !scheme_ok {
            bail!(
                "amp.url must be an http:// or https:// URL, got {:?}",
                amp.url
            );
        }
        if amp.username.trim().is_empty() {
            bail!("amp.username cannot be empty when amp is configured");
        }
        if amp.password.expose_secret().trim().is_empty() {
            bail!("amp.password cannot be empty when amp is configured");
        }
    }

    if config
        .finnhub
        .as_ref()
        .is_some_and(|f| f.api_key.expose_secret().trim().is_empty())
    {
        bail!("finnhub.api_key cannot be empty when finnhub is configured");
    }

    if let Some(github) = config.github.as_ref() {
        if !crate::github::is_valid_repo(&github.repo) {
            bail!("github.repo must be \"owner/name\", got {:?}", github.repo);
        }
        if github.app_id == 0 {
            bail!("github.app_id must be set when github is configured");
        }
        if let Err(error) = crate::github::parse_private_key(github.private_key.expose_secret()) {
            bail!(
                "github.private_key must be an inline RSA private key PEM (PKCS#1 or PKCS#8): {error:#}"
            );
        }
    }

    if let Some(lf) = config.langfuse.as_ref() {
        if lf.public_key.expose_secret().trim().is_empty() {
            bail!("langfuse.public_key cannot be empty when langfuse is configured");
        }
        if lf.secret_key.expose_secret().trim().is_empty() {
            bail!("langfuse.secret_key cannot be empty when langfuse is configured");
        }
    }

    if let Some(otel) = config.otel.as_ref() {
        // Joined with `/v1/<signal>` by the exporter, so it must be a bare
        // base URL. Credentials belong in `auth_header`, not the URL, which is
        // logged at startup.
        let endpoint_ok = otel.endpoint.trim() == otel.endpoint
            && reqwest::Url::parse(&otel.endpoint).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https")
                    && u.has_host()
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.query().is_none()
                    && u.fragment().is_none()
            });
        if !endpoint_ok {
            // Not echoed: a rejected URL may carry a password in its userinfo.
            bail!(
                "otel.endpoint must be an http:// or https:// base URL without \
                 surrounding whitespace, credentials, query or fragment"
            );
        }
        if let Some(auth) = otel.auth_header.as_ref() {
            let value = auth.expose_secret();
            if value.trim().is_empty() || reqwest::header::HeaderValue::from_str(value).is_err() {
                bail!("otel.auth_header must be a non-empty, valid HTTP header value when set");
            }
        }
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
        if config.web.public_url.len() > 256 {
            bail!(
                "web.public_url must be at most 256 bytes when web.enabled = true (got {})",
                config.web.public_url.len()
            );
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
    fn web_enabled_rejects_public_url_over_256_bytes() {
        let mut cfg = Configuration::test_default();
        cfg.web.enabled = true;
        cfg.web.session_secret = secrecy::SecretString::new("00".repeat(32).into());
        cfg.web.public_url = format!("https://bot.example/{}", "a".repeat(236));
        validate_config(&cfg).expect("256-byte public_url must be accepted");
        cfg.web.public_url.push('a');
        assert_eq!(cfg.web.public_url.len(), 257);

        let err = validate_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("web.public_url"), "{err}");
        assert!(err.contains("256"), "{err}");
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

    #[test]
    fn empty_finnhub_api_key_is_rejected() {
        let cfg: Configuration = toml::from_str(
            r#"
            [twitch]
            channel = "c"
            username = "u"
            refresh_token = "r"
            client_id = "i"
            client_secret = "s"

            [finnhub]
            api_key = ""
        "#,
        )
        .expect("parse");
        let err = validate_config(&cfg).expect_err("empty key must fail");
        assert!(
            err.to_string().contains("finnhub.api_key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn empty_ipinfo_token_is_rejected() {
        let mut cfg = Configuration::test_default();
        cfg.ipinfo = Some(IpinfoBootstrap {
            token: SecretString::new(" ".into()),
        });
        let err = validate_config(&cfg).expect_err("empty token must fail");
        assert!(
            err.to_string().contains("ipinfo.token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn amp_blank_password_rejected() {
        let mut cfg = Configuration::test_default();
        cfg.amp = Some(AmpBootstrap {
            url: "http://amp:8080".to_owned(),
            username: "bot".to_owned(),
            password: SecretString::new(" ".into()),
        });
        let err = validate_config(&cfg).expect_err("blank password must fail");
        assert!(
            err.to_string().contains("amp.password"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn amp_url_without_http_scheme_rejected() {
        for url in [
            "",
            "192.0.2.1:8080",
            "ftp://amp:8080",
            "amp",
            "http://amp:8080 ",
            " http://amp:8080",
            "http://amp:8080/?x=1",
            "http://amp:8080/#/",
        ] {
            let mut cfg = Configuration::test_default();
            cfg.amp = Some(AmpBootstrap {
                url: url.to_owned(),
                username: "bot".to_owned(),
                password: SecretString::new("pw".into()),
            });
            let err = validate_config(&cfg).expect_err(url);
            assert!(err.to_string().contains("amp.url"), "{url}: {err}");
        }
        let mut cfg = Configuration::test_default();
        cfg.amp = Some(AmpBootstrap {
            url: "https://amp.example:8080".to_owned(),
            username: "bot".to_owned(),
            password: SecretString::new("pw".into()),
        });
        validate_config(&cfg).expect("https accepted");
    }

    #[test]
    fn empty_langfuse_keys_are_rejected() {
        let mut cfg = Configuration::test_default();
        cfg.langfuse = Some(LangfuseBootstrap {
            public_key: SecretString::new("   ".into()),
            secret_key: SecretString::new("sk".into()),
            host: None,
        });
        let err = validate_config(&cfg).expect_err("empty public_key must fail");
        assert!(
            err.to_string().contains("langfuse.public_key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn otel_endpoint_must_be_bare_http_base_url() {
        let otel = |endpoint: &str| OtelBootstrap {
            endpoint: endpoint.to_owned(),
            auth_header: None,
        };
        for bad in [
            "",
            "alloy:4318",
            "grpc://alloy:4317",
            " http://alloy:4318",
            "http://user:pw@alloy:4318",
            "http://alloy:4318/?token=x",
        ] {
            let mut cfg = Configuration::test_default();
            cfg.otel = Some(otel(bad));
            let err = validate_config(&cfg).expect_err(bad);
            assert!(err.to_string().contains("otel.endpoint"), "{bad}: {err}");
            assert!(!err.to_string().contains("user:pw"), "{bad}: {err}");
        }
        let mut cfg = Configuration::test_default();
        cfg.otel = Some(OtelBootstrap {
            auth_header: Some(SecretString::new("Basic\nx".into())),
            ..otel("http://alloy:4318")
        });
        let err = validate_config(&cfg).expect_err("newline in header");
        assert!(err.to_string().contains("otel.auth_header"), "{err}");
        cfg.otel = Some(otel("http://alloy:4318"));
        validate_config(&cfg).expect("plain LAN collector accepted");
    }

    fn github(repo: &str, key: &str) -> GithubBootstrap {
        GithubBootstrap {
            repo: repo.to_owned(),
            app_id: 42,
            private_key: SecretString::new(key.into()),
        }
    }

    #[test]
    fn github_section_parses_and_validates() {
        let cfg: Configuration = toml::from_str(&format!(
            r#"
            [twitch]
            channel = "c"
            username = "u"
            refresh_token = "r"
            client_id = "i"
            client_secret = "s"

            [github]
            repo = "Chronophylos/twitch-1337"
            app_id = 42
            private_key = """
{}"""
        "#,
            crate::github::test_support::TEST_PRIVATE_KEY_PEM
        ))
        .expect("parse");
        let gh = cfg.github.as_ref().expect("github present");
        assert_eq!(gh.repo, "Chronophylos/twitch-1337");
        assert_eq!(gh.app_id, 42);
        validate_config(&cfg).expect("valid github section");
    }

    #[test]
    fn github_repo_must_be_owner_slash_name() {
        let key = crate::github::test_support::TEST_PRIVATE_KEY_PEM;
        for bad in [
            "",
            "twitch-1337",
            "/name",
            "owner/",
            "a/b/c",
            " a/b",
            "a/b ",
            "a b/c",
        ] {
            let mut cfg = Configuration::test_default();
            cfg.github = Some(github(bad, key));
            let err = validate_config(&cfg).expect_err(bad);
            assert!(err.to_string().contains("github.repo"), "{bad}: {err}");
        }
    }

    #[test]
    fn github_private_key_must_parse() {
        for bad in [
            "",
            "not a pem",
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n",
        ] {
            let mut cfg = Configuration::test_default();
            cfg.github = Some(github("o/r", bad));
            let err = validate_config(&cfg).expect_err(bad);
            assert!(err.to_string().contains("github.private_key"), "{err}");
        }
        let mut cfg = Configuration::test_default();
        cfg.github = Some(github(
            "o/r",
            include_str!("github/fixtures/test_key_pkcs8.pem"),
        ));
        validate_config(&cfg).expect("PKCS#8 accepted");
    }
}
