//! Resolves a profile name to a live [`LlmClient`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use llm::{LlmClient, OllamaClient, OpenAiClient};
use secrecy::{ExposeSecret as _, SecretString};
use tracing::{error, info, warn};

use crate::APP_USER_AGENT;
use crate::config::AiBootstrap;
use crate::settings::SettingsHandle;
use crate::settings::ai::AiProfile;
use crate::settings::provider::AiProvider;

/// One client per endpoint identity. Model, timeout, effort and tier are
/// per-request, so two profiles differing only in those share a client.
type CacheKey = (AiProvider, Option<String>, Option<String>);

pub struct LlmRegistry {
    api_key: Option<SecretString>,
    keys: HashMap<String, SecretString>,
    settings: SettingsHandle,
    // ponytail: no invalidation. Entries are keyed by endpoint identity and
    // a reqwest client is cheap to keep; a profile edited into nonexistence
    // just leaves a few idle connection pools behind until restart.
    cache: Mutex<HashMap<CacheKey, Arc<dyn LlmClient>>>,
    /// Set by [`LlmRegistry::fixed`] — bypasses construction entirely.
    fixed: Option<Arc<dyn LlmClient>>,
}

impl LlmRegistry {
    pub fn new(bootstrap: Option<&AiBootstrap>, settings: SettingsHandle) -> Self {
        Self {
            api_key: bootstrap.map(|b| b.api_key.clone()),
            keys: bootstrap.map(|b| b.keys.clone()).unwrap_or_default(),
            settings,
            cache: Mutex::new(HashMap::new()),
            fixed: None,
        }
    }

    /// Test-injection constructor: every `resolve` returns `client`, paired
    /// with the profile the settings snapshot selects. Public because the
    /// integration tests in `crates/core/tests/` cannot reach `#[cfg(test)]`
    /// items in this crate.
    pub fn fixed(client: Arc<dyn LlmClient>, settings: SettingsHandle) -> Self {
        Self {
            api_key: Some(SecretString::from("test")),
            keys: HashMap::new(),
            settings,
            cache: Mutex::new(HashMap::new()),
            fixed: Some(client),
        }
    }

    /// True when the active profile can currently produce a client.
    pub fn is_usable(&self) -> bool {
        self.resolve(None).is_some()
    }

    /// `profile: None` = the active profile. A name that no longer exists
    /// falls back to the active profile rather than disabling the agent —
    /// deleting a profile should not silently kill the dreamer.
    pub fn resolve(&self, profile: Option<&str>) -> Option<(Arc<dyn LlmClient>, AiProfile)> {
        let snapshot = self.settings.load();
        let ai = &snapshot.ai;
        let prof = match profile {
            Some(name) => ai.by_name(name).unwrap_or_else(|| {
                warn!(profile = name, "profile not found, using active profile");
                ai.active()
            }),
            None => ai.active(),
        }
        .clone();

        if prof.model.trim().is_empty() {
            error!(
                profile = %prof.name,
                "AI profile has an empty model, AI disabled (set it at /ai-profiles)"
            );
            return None;
        }
        let client = self.client_for(&prof)?;
        Some((client, prof))
    }

    fn client_for(&self, prof: &AiProfile) -> Option<Arc<dyn LlmClient>> {
        if let Some(fixed) = &self.fixed {
            return Some(fixed.clone());
        }
        let key = (prof.provider, prof.base_url.clone(), prof.key.clone());
        let mut cache = self.cache.lock().expect("llm registry cache poisoned");
        if let Some(hit) = cache.get(&key) {
            return Some(hit.clone());
        }

        let secret = match &prof.key {
            Some(name) => match self.keys.get(name) {
                Some(s) => Some(s),
                None => {
                    error!(
                        profile = %prof.name,
                        key = %name,
                        "profile names a key with no [ai.keys] entry in config.toml"
                    );
                    return None;
                }
            },
            None => self.api_key.as_ref(),
        };

        let built = if prof.provider.uses_openai_client() {
            let secret = secret.or_else(|| {
                error!(profile = %prof.name, "no API key configured");
                None
            })?;
            OpenAiClient::new(
                secret.expose_secret(),
                prof.base_url
                    .as_deref()
                    .or(prof.provider.openai_compat_base_url()),
                APP_USER_AGENT,
                prof.provider.is_openrouter(),
            )
            .map(|c| Arc::new(c) as Arc<dyn LlmClient>)
        } else {
            OllamaClient::new(
                prof.base_url
                    .as_deref()
                    .or(prof.provider.caps().default_base_url),
                APP_USER_AGENT,
            )
            .map(|c| Arc::new(c) as Arc<dyn LlmClient>)
        };

        match built {
            Ok(client) => {
                info!(
                    profile = %prof.name,
                    provider = prof.provider.as_str(),
                    model = %prof.model,
                    "LLM client initialized"
                );
                cache.insert(key, client.clone());
                Some(client)
            }
            Err(e) => {
                error!(error = ?e, profile = %prof.name, "Failed to initialize LLM client");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(profiles: Vec<AiProfile>, active: &str) -> SettingsHandle {
        let mut s = crate::settings::Settings::compiled_defaults();
        s.ai.profiles = profiles;
        s.ai.active_profile = active.to_string();
        Arc::new(arc_swap::ArcSwap::from_pointee(s))
    }

    fn p(name: &str, provider: AiProvider, model: &str) -> AiProfile {
        AiProfile {
            name: name.into(),
            key: None,
            provider,
            base_url: Some("https://example.test/v1".into()),
            model: model.into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
        }
    }

    fn boot() -> AiBootstrap {
        AiBootstrap {
            api_key: "root-key".into(),
            keys: HashMap::from([("alt".to_string(), SecretString::from("alt-key"))]),
        }
    }

    #[test]
    fn resolves_the_active_profile_when_none_is_named() {
        crate::install_crypto_provider();
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(
                vec![
                    p("a", AiProvider::OpenRouter, "m-a"),
                    p("b", AiProvider::Ollama, "m-b"),
                ],
                "b",
            ),
        );
        let (_, prof) = r.resolve(None).unwrap();
        assert_eq!(prof.name, "b");
        assert_eq!(prof.model, "m-b");
    }

    #[test]
    fn resolves_a_named_profile_for_a_subagent() {
        crate::install_crypto_provider();
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(
                vec![
                    p("a", AiProvider::OpenRouter, "m-a"),
                    p("b", AiProvider::Ollama, "m-b"),
                ],
                "b",
            ),
        );
        assert_eq!(r.resolve(Some("a")).unwrap().1.model, "m-a");
    }

    #[test]
    fn a_stale_subagent_profile_falls_back_to_active() {
        crate::install_crypto_provider();
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(vec![p("a", AiProvider::Ollama, "m-a")], "a"),
        );
        assert_eq!(r.resolve(Some("deleted")).unwrap().1.name, "a");
    }

    #[test]
    fn the_same_endpoint_reuses_one_client() {
        crate::install_crypto_provider();
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(
                vec![
                    p("a", AiProvider::OpenRouter, "m-a"),
                    p("b", AiProvider::OpenRouter, "m-b"),
                ],
                "a",
            ),
        );
        let (c1, _) = r.resolve(Some("a")).unwrap();
        let (c2, _) = r.resolve(Some("b")).unwrap();
        assert!(
            Arc::ptr_eq(&c1, &c2),
            "same provider+url+key must share a client"
        );
    }

    #[test]
    fn a_different_endpoint_gets_its_own_client() {
        crate::install_crypto_provider();
        let mut other = p("b", AiProvider::OpenRouter, "m-b");
        other.base_url = Some("https://other.test/v1".into());
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(vec![p("a", AiProvider::OpenRouter, "m-a"), other], "a"),
        );
        let (c1, _) = r.resolve(Some("a")).unwrap();
        let (c2, _) = r.resolve(Some("b")).unwrap();
        assert!(!Arc::ptr_eq(&c1, &c2));
    }

    #[test]
    fn a_blank_model_disables_the_profile() {
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(vec![p("a", AiProvider::OpenRouter, "")], "a"),
        );
        assert!(r.resolve(None).is_none());
        assert!(!r.is_usable());
    }

    #[test]
    fn no_bootstrap_means_no_client() {
        let r = LlmRegistry::new(None, handle(vec![p("a", AiProvider::OpenRouter, "m")], "a"));
        assert!(r.resolve(None).is_none());
        assert!(!r.is_usable());
    }

    #[test]
    fn an_unknown_key_name_disables_the_profile() {
        let mut prof = p("a", AiProvider::OpenRouter, "m");
        prof.key = Some("missing".into());
        let r = LlmRegistry::new(Some(&boot()), handle(vec![prof], "a"));
        assert!(r.resolve(None).is_none());
    }
}
