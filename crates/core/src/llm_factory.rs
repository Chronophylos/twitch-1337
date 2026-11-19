//! Resolves a profile name to a live [`LlmClient`].

use std::collections::HashMap;
use std::sync::Arc;

use llm::{LlmClient, OllamaClient, OpenAiClient};
use parking_lot::Mutex;
use secrecy::{ExposeSecret as _, SecretString};
use tracing::{error, info, warn};

use crate::APP_USER_AGENT;
use crate::config::AiBootstrap;
use crate::settings::SettingsHandle;
use crate::settings::ai::{AiProfile, ProfileKind};
use crate::settings::provider::AiProvider;

/// One client per endpoint identity. Model, timeout, effort and tier are
/// per-request, so two profiles differing only in those share a client.
type CacheKey = (AiProvider, Option<String>, Option<String>);

pub struct LlmRegistry {
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
        if let Some(boot) = bootstrap
            && boot
                .api_key
                .as_ref()
                .is_some_and(|k| !k.expose_secret().trim().is_empty())
        {
            warn!("[ai].api_key is ignored; move it to [ai.keys] and set each profile's key name");
        }
        Self {
            keys: bootstrap.map(|b| b.keys.clone()).unwrap_or_default(),
            settings,
            cache: Mutex::new(HashMap::new()),
            fixed: None,
        }
    }

    /// Test-injection constructor: every successful [`Self::resolve_named`]
    /// returns `client`, paired with the named profile. Public because the
    /// integration tests in `crates/core/tests/` cannot reach `#[cfg(test)]`
    /// items in this crate.
    pub fn fixed(client: Arc<dyn LlmClient>, settings: SettingsHandle) -> Self {
        Self {
            keys: HashMap::new(),
            settings,
            cache: Mutex::new(HashMap::new()),
            fixed: Some(client),
        }
    }

    /// Resolve a profile by name. Missing and blank names return `None`.
    pub fn resolve_named(&self, name: &str) -> Option<(Arc<dyn LlmClient>, AiProfile)> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let snapshot = self.settings.load();
        let Some(prof) = snapshot.ai.by_name(name).cloned() else {
            warn!(profile = name, "named profile not found");
            return None;
        };
        if prof.kind == ProfileKind::Decision {
            warn!(
                profile = %prof.name,
                "decision profile has no chat client, named resolve skipped"
            );
            return None;
        }
        if prof.model.trim().is_empty() {
            error!(
                profile = %prof.name,
                "AI profile has an empty model, named resolve skipped (set it at /ai-profiles)"
            );
            return None;
        }
        let client = self.client_for(&prof)?;
        Some((client, prof))
    }

    /// The API key a profile should use: a named `[ai.keys]` entry.
    /// `Err` means a named key was requested but has no `[ai.keys]` entry
    /// (a hard failure); `Ok(None)` means no key is configured at all, which
    /// is fine for providers that don't need one (e.g. Ollama). Shared by
    /// `client_for` and [`Self::secret_for`] so the two lookups cannot drift.
    fn key_for(&self, prof: &AiProfile) -> Result<Option<&SecretString>, ()> {
        match &prof.key {
            Some(name) => match self.keys.get(name) {
                Some(s) => Ok(Some(s)),
                None => {
                    error!(
                        profile = %prof.name,
                        key = %name,
                        "profile names a key with no [ai.keys] entry in config.toml"
                    );
                    Err(())
                }
            },
            None => Ok(None),
        }
    }

    /// Test-injection companion to [`Self::fixed`]: give it `[ai.keys]` so
    /// [`Self::secret_for`] resolves named keys.
    pub fn with_keys(mut self, keys: HashMap<String, SecretString>) -> Self {
        self.keys = keys;
        self
    }

    /// The API key a profile should use, by the same rules `client_for`
    /// applies: a named `[ai.keys]` entry. `MediaClient` needs the raw secret
    /// because it posts to `/chat/completions` itself rather than going
    /// through `LlmClient`.
    pub fn secret_for(&self, prof: &AiProfile) -> Option<SecretString> {
        self.key_for(prof).ok().flatten().cloned()
    }

    fn client_for(&self, prof: &AiProfile) -> Option<Arc<dyn LlmClient>> {
        if let Some(fixed) = &self.fixed {
            return Some(fixed.clone());
        }
        let key = (prof.provider, prof.base_url.clone(), prof.key.clone());
        let mut cache = self.cache.lock();
        if let Some(hit) = cache.get(&key) {
            return Some(hit.clone());
        }

        let secret = self.key_for(prof).ok()?;

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

    fn handle(profiles: Vec<AiProfile>, aurora: &str) -> SettingsHandle {
        let mut s = crate::settings::Settings::compiled_defaults();
        s.ai.profiles = profiles;
        s.ai.aurora.profile = aurora.to_string();
        SettingsHandle::from_settings(s)
    }

    fn p(name: &str, provider: AiProvider, model: &str) -> AiProfile {
        AiProfile {
            name: name.into(),
            key: Some("alt".into()),
            provider,
            base_url: Some("https://example.test/v1".into()),
            model: model.into(),
            timeout: 30,
            reasoning_effort: None,
            service_tier: None,
            ..Default::default()
        }
    }

    fn boot() -> AiBootstrap {
        AiBootstrap {
            api_key: None,
            keys: HashMap::from([("alt".to_string(), SecretString::from("alt-key"))]),
        }
    }

    #[test]
    fn resolve_named_hits_the_named_profile() {
        crate::install_crypto_provider();
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(
                vec![
                    p("a", AiProvider::OpenRouter, "m-a"),
                    p("b", AiProvider::Ollama, "m-b"),
                ],
                "a",
            ),
        );
        let (_, prof) = r.resolve_named("b").unwrap();
        assert_eq!(prof.name, "b");
        assert_eq!(prof.model, "m-b");
    }

    #[test]
    fn resolve_named_rejects_unknown_or_blank_names() {
        crate::install_crypto_provider();
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(vec![p("a", AiProvider::Ollama, "m-a")], "a"),
        );
        assert!(r.resolve_named("deleted").is_none());
        assert!(r.resolve_named("").is_none());
        assert!(r.resolve_named("   ").is_none());
    }

    #[test]
    fn resolve_named_refuses_decision_profile() {
        crate::install_crypto_provider();
        let mut d = p("d", AiProvider::OpenRouter, "m-d");
        d.kind = ProfileKind::Decision;
        let r = LlmRegistry::new(Some(&boot()), handle(vec![d], "d"));
        assert!(r.resolve_named("d").is_none());
    }

    #[test]
    fn resolve_named_skips_an_empty_model() {
        crate::install_crypto_provider();
        let r = LlmRegistry::new(
            Some(&boot()),
            handle(vec![p("empty", AiProvider::Ollama, "")], "empty"),
        );
        assert!(r.resolve_named("empty").is_none());
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
        let (c1, _) = r.resolve_named("a").unwrap();
        let (c2, _) = r.resolve_named("b").unwrap();
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
        let (c1, _) = r.resolve_named("a").unwrap();
        let (c2, _) = r.resolve_named("b").unwrap();
        assert!(!Arc::ptr_eq(&c1, &c2));
    }

    #[test]
    fn no_bootstrap_means_no_client() {
        let r = LlmRegistry::new(None, handle(vec![p("a", AiProvider::OpenRouter, "m")], "a"));
        assert!(r.resolve_named("a").is_none());
    }

    #[test]
    fn a_missing_key_name_disables_an_openai_profile() {
        let mut prof = p("a", AiProvider::OpenRouter, "m");
        prof.key = None;
        let r = LlmRegistry::new(Some(&boot()), handle(vec![prof], "a"));
        assert!(r.resolve_named("a").is_none());
    }

    #[test]
    fn leftover_root_api_key_is_not_a_fallback() {
        let mut boot = boot();
        boot.api_key = Some(SecretString::from("legacy-root"));
        let mut prof = p("a", AiProvider::OpenRouter, "m");
        prof.key = None;
        let r = LlmRegistry::new(Some(&boot), handle(vec![prof], "a"));
        assert!(
            r.resolve_named("a").is_none(),
            "[ai].api_key must not stand in for a named [ai.keys] entry"
        );
    }

    #[test]
    fn an_unknown_key_name_disables_the_profile() {
        let mut prof = p("a", AiProvider::OpenRouter, "m");
        prof.key = Some("missing".into());
        let r = LlmRegistry::new(Some(&boot()), handle(vec![prof], "a"));
        assert!(r.resolve_named("a").is_none());
    }
}
