//! Resolves a profile name to a live [`LlmClient`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use llm::{
    ChatCompletionRequest, LlmClient, LlmError, Message, OllamaClient, OpenAiClient, TraceIds,
};
use parking_lot::Mutex;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use thiserror::Error;
use tracing::{error, info, warn};

use crate::APP_USER_AGENT;
use crate::config::AiBootstrap;
use crate::settings::SettingsHandle;
use crate::settings::ai::{AiProfile, ProfileKind};
use crate::settings::provider::{AiProvider, ModelListing};
use crate::util::http::{BodyError, SendTraced as _, read_capped};

/// One client per endpoint identity. Model, timeout, effort and tier are
/// per-request, so two profiles differing only in those share a client.
type CacheKey = (AiProvider, Option<String>, Option<String>);

/// A failed non-persisting profile check. Key-selection failures stay typed so
/// each Surface can explain them without inspecting log strings.
#[derive(Debug, Error)]
pub enum ProfileTestError {
    #[error("profile names key {0:?}, but config.toml has no matching [ai.keys] entry")]
    UnknownKey(String),
    #[error("provider requires an API key")]
    MissingKey,
    #[error("profile request failed")]
    Llm(#[from] LlmError),
}

const MAX_LISTED_MODELS: usize = 2000;
const MAX_MODEL_LIST_BYTES: u64 = 8 * 1024 * 1024;

/// One native autocomplete suggestion. `label` carries optional provider
/// metadata; the input value always remains the exact upstream model id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedModel {
    pub id: String,
    pub label: Option<String>,
}

/// A model-listing failure reduced to non-sensitive kinds. In particular,
/// provider bodies, reqwest errors and key material are deliberately not
/// retained: dashboard responses and logs must never expose them.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ModelListingError {
    #[error("unknown key name")]
    UnknownKey,
    #[error("provider requires an API key")]
    MissingKey,
    #[error("provider requires a base URL")]
    MissingBaseUrl,
    #[error("provider does not support model listing")]
    Unsupported,
    #[error("invalid provider request")]
    InvalidRequest,
    #[error("model listing timed out")]
    Timeout,
    #[error("provider connection failed")]
    Connect,
    #[error("provider request failed")]
    Transport,
    #[error("provider returned HTTP status {0}")]
    HttpStatus(u16),
    #[error("provider returned a malformed model list")]
    MalformedResponse,
    #[error("provider model list exceeded the 8 MiB limit")]
    ResponseTooLarge,
}

impl ModelListingError {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::UnknownKey => "unknown_key",
            Self::MissingKey => "missing_key",
            Self::MissingBaseUrl => "missing_base_url",
            Self::Unsupported => "unsupported",
            Self::InvalidRequest => "invalid_request",
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Transport => "transport",
            Self::HttpStatus(_) => "http_status",
            Self::MalformedResponse => "malformed_response",
            Self::ResponseTooLarge => "response_too_large",
        }
    }

    pub const fn status(&self) -> Option<u16> {
        match self {
            Self::HttpStatus(status) => Some(*status),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct OpenRouterModels {
    data: Vec<OpenRouterModel>,
}

#[derive(Deserialize)]
struct OpenRouterModel {
    id: String,
    name: String,
    context_length: Option<u64>,
    architecture: OpenRouterArchitecture,
}

#[derive(Deserialize)]
struct OpenRouterArchitecture {
    input_modalities: Vec<String>,
    output_modalities: Vec<String>,
}

#[derive(Deserialize)]
struct OpenAiModels {
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    id: String,
}

#[derive(Deserialize)]
struct OllamaModels {
    models: Vec<OllamaModel>,
}

#[derive(Deserialize)]
struct OllamaModel {
    name: String,
    size: u64,
}

fn model_request_failure(error: reqwest::Error) -> ModelListingError {
    if error.is_timeout() {
        ModelListingError::Timeout
    } else if error.is_connect() {
        ModelListingError::Connect
    } else if error.is_builder() {
        ModelListingError::InvalidRequest
    } else {
        ModelListingError::Transport
    }
}

fn log_model_listing_failure(error: &ModelListingError) {
    if let Some(status) = error.status() {
        warn!(
            kind = error.kind(),
            status = status,
            "AI model listing failed"
        );
    } else {
        warn!(kind = error.kind(), "AI model listing failed");
    }
}

fn sort_and_truncate_models(mut models: Vec<ListedModel>) -> Vec<ListedModel> {
    models.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    models.truncate(MAX_LISTED_MODELS);
    models
}

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

    /// Make one minimal completion with `prof` exactly as supplied, without
    /// resolving or mutating saved settings. Returns the provider round-trip
    /// latency; request construction and response parsing use the same clients
    /// as normal turns, including reasoning effort and service tier.
    #[expect(
        clippy::map_err_ignore,
        reason = "tokio Elapsed carries no information beyond the profile timeout"
    )]
    pub async fn test_profile(&self, prof: &AiProfile) -> Result<Duration, ProfileTestError> {
        let client = self.build_client(prof)?;
        let request = ChatCompletionRequest {
            model: prof.model.clone(),
            messages: vec![Message::user("Reply with OK.")],
            reasoning_effort: prof.reasoning_effort.clone(),
            service_tier: prof.service_tier.clone(),
            trace: TraceIds::default(),
        };
        let started = tokio::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(prof.timeout),
            crate::util::observability::metrics::with_llm_scope(
                "profile_test",
                &prof.name,
                client.chat_completion(request),
            ),
        )
        .await
        .map_err(|_| ProfileTestError::Llm(LlmError::Timeout))??;
        Ok(started.elapsed())
    }

    /// Fetch a fresh model list using the submitted profile, without saving
    /// the profile or caching either the HTTP client or the returned models.
    pub async fn list_models(
        &self,
        prof: &AiProfile,
    ) -> Result<Vec<ListedModel>, ModelListingError> {
        let result =
            match tokio::time::timeout(Duration::from_secs(prof.timeout), self.fetch_models(prof))
                .await
            {
                Ok(result) => result,
                Err(_) => Err(ModelListingError::Timeout),
            };
        if let Err(error) = &result {
            log_model_listing_failure(error);
        }
        result
    }

    #[expect(
        clippy::map_err_ignore,
        reason = "model listing deliberately erases provider details so keys and response bodies cannot leak"
    )]
    async fn fetch_models(&self, prof: &AiProfile) -> Result<Vec<ListedModel>, ModelListingError> {
        let capability = prof.provider.caps().model_listing;
        let suffix = match capability {
            ModelListing::OpenRouter | ModelListing::OpenAiStyle => "/models",
            ModelListing::OllamaTags => "/api/tags",
            ModelListing::None => return Err(ModelListingError::Unsupported),
        };
        let base_url = prof
            .base_url
            .as_deref()
            .or(prof.provider.caps().default_base_url)
            .ok_or(ModelListingError::MissingBaseUrl)?;
        let url = format!("{}{suffix}", base_url.trim_end_matches('/'));

        // Resolve directly rather than through `key_for`: that helper logs
        // unknown key names itself, while listing has one sanitized warn at
        // its outer boundary.
        let secret = match &prof.key {
            Some(name) => Some(self.keys.get(name).ok_or(ModelListingError::UnknownKey)?),
            None => None,
        };
        if matches!(prof.provider, AiProvider::OpenAi | AiProvider::OpenAiCompatible)
            && secret.is_none()
        {
            return Err(ModelListingError::MissingKey);
        }

        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()
            .map_err(model_request_failure)?;
        let mut request = http.get(url);
        if prof.provider.uses_openai_client()
            && let Some(secret) = secret
        {
            let mut authorization =
                HeaderValue::from_str(&format!("Bearer {}", secret.expose_secret()))
                    .map_err(|_| ModelListingError::InvalidRequest)?;
            authorization.set_sensitive(true);
            request = request.header(AUTHORIZATION, authorization);
        }

        let response = request.send_traced().await.map_err(model_request_failure)?;
        if !response.status().is_success() {
            return Err(ModelListingError::HttpStatus(response.status().as_u16()));
        }
        let body = read_capped(response, MAX_MODEL_LIST_BYTES)
            .await
            .map_err(|error| match error {
                BodyError::TooLarge { .. } => ModelListingError::ResponseTooLarge,
                BodyError::Read(error) => model_request_failure(error),
            })?;
        match capability {
            ModelListing::OpenRouter => {
                let listing: OpenRouterModels = serde_json::from_slice(&body)
                    .map_err(|_| ModelListingError::MalformedResponse)?;
                let models = listing
                    .data
                    .into_iter()
                    .filter(|model| {
                        model
                            .architecture
                            .output_modalities
                            .iter()
                            .any(|modality| modality == "text")
                    })
                    .map(|model| {
                        let context = match model.context_length {
                            Some(length) => format!("{length} tokens"),
                            None => "unknown context".to_owned(),
                        };
                        let image_input = model
                            .architecture
                            .input_modalities
                            .iter()
                            .any(|modality| modality == "image");
                        let image_marker = if image_input { " · image input" } else { "" };
                        let label =
                            format!("{} — {} · {context}{image_marker}", model.id, model.name);
                        ListedModel {
                            id: model.id,
                            label: Some(label),
                        }
                    })
                    .collect();
                Ok(sort_and_truncate_models(models))
            }
            ModelListing::OpenAiStyle => {
                let listing: OpenAiModels = serde_json::from_slice(&body)
                    .map_err(|_| ModelListingError::MalformedResponse)?;
                let models = listing
                    .data
                    .into_iter()
                    .map(|model| ListedModel {
                        id: model.id,
                        label: None,
                    })
                    .collect();
                Ok(sort_and_truncate_models(models))
            }
            ModelListing::OllamaTags => {
                let listing: OllamaModels = serde_json::from_slice(&body)
                    .map_err(|_| ModelListingError::MalformedResponse)?;
                let models = listing
                    .models
                    .into_iter()
                    .map(|model| {
                        let label = format!("{} — {} bytes", model.name, model.size);
                        ListedModel {
                            id: model.name,
                            label: Some(label),
                        }
                    })
                    .collect();
                Ok(sort_and_truncate_models(models))
            }
            ModelListing::None => Err(ModelListingError::Unsupported),
        }
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

        match self.build_client(prof) {
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
                crate::log_handled!(error, e, profile = %prof.name, "Failed to initialize LLM client");
                None
            }
        }
    }

    /// Build an uncached client while retaining the reason construction failed.
    /// Normal named resolution turns this into `None`; the dashboard profile
    /// test instead presents the typed rejection to its owner.
    fn build_client(&self, prof: &AiProfile) -> Result<Arc<dyn LlmClient>, ProfileTestError> {
        if let Some(fixed) = &self.fixed {
            return Ok(fixed.clone());
        }
        let secret = self
            .key_for(prof)
            .map_err(|()| ProfileTestError::UnknownKey(prof.key.clone().unwrap_or_default()))?;

        if prof.provider.uses_openai_client() {
            let secret = secret.ok_or(ProfileTestError::MissingKey)?;
            Ok(Arc::new(OpenAiClient::new(
                secret.expose_secret(),
                prof.base_url
                    .as_deref()
                    .or(prof.provider.openai_compat_base_url()),
                APP_USER_AGENT,
                prof.provider.is_openrouter(),
                prof.provider.as_str(),
            )?))
        } else {
            Ok(Arc::new(OllamaClient::new(
                prof.base_url
                    .as_deref()
                    .or(prof.provider.caps().default_base_url),
                APP_USER_AGENT,
            )?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header as request_header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    fn listing_registry() -> LlmRegistry {
        LlmRegistry::new(Some(&boot()), handle(Vec::new(), ""))
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

    #[tokio::test]
    async fn openrouter_listing_has_metadata_and_drops_non_text_outputs() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(request_header("authorization", "Bearer alt-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "vendor/vision",
                        "name": "Vision Model",
                        "context_length": 131072,
                        "architecture": {
                            "input_modalities": ["text", "image"],
                            "output_modalities": ["text"]
                        }
                    },
                    {
                        "id": "vendor/image-generator",
                        "name": "Image Generator",
                        "context_length": 8192,
                        "architecture": {
                            "input_modalities": ["text"],
                            "output_modalities": ["image"]
                        }
                    },
                    {
                        "id": "vendor/chat",
                        "name": "Chat Model",
                        "context_length": null,
                        "architecture": {
                            "input_modalities": ["text"],
                            "output_modalities": ["text"]
                        }
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut prof = p("draft", AiProvider::OpenRouter, "unused");
        prof.base_url = Some(server.uri());

        let models = listing_registry().list_models(&prof).await.unwrap();
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["vendor/chat", "vendor/vision"]);
        let vision = &models[1];
        let label = vision.label.as_deref().unwrap();
        assert!(label.contains("vendor/vision"), "{label}");
        assert!(label.contains("Vision Model"), "{label}");
        assert!(label.contains("131072 tokens"), "{label}");
        assert!(label.contains("image input"), "{label}");
        assert!(
            models[0]
                .label
                .as_deref()
                .unwrap()
                .contains("unknown context")
        );
    }

    #[tokio::test]
    async fn openrouter_listing_works_without_a_key_or_authorization_header() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": []
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut prof = p("draft", AiProvider::OpenRouter, "unused");
        prof.base_url = Some(server.uri());
        prof.key = None;

        let models = listing_registry().list_models(&prof).await.unwrap();
        assert!(models.is_empty());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn openai_listing_sorts_ids_without_filtering_model_types() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(request_header("authorization", "Bearer alt-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    { "id": "tts-1", "object": "model", "created": 1686935002, "owned_by": "openai" },
                    { "id": "gpt-5", "object": "model", "created": 1686935002, "owned_by": "openai" },
                    { "id": "text-embedding-3-large", "object": "model", "created": 1686935002, "owned_by": "openai" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut prof = p("draft", AiProvider::OpenAi, "unused");
        prof.base_url = Some(server.uri());

        let models = listing_registry().list_models(&prof).await.unwrap();
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["gpt-5", "text-embedding-3-large", "tts-1"]);
        assert!(models.iter().all(|model| model.label.is_none()));
    }

    #[tokio::test]
    async fn ollama_listing_uses_native_tags_and_sorts_ids() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {
                        "name": "qwen3:8b",
                        "model": "qwen3:8b",
                        "modified_at": "2025-10-03T23:34:03.409490317-07:00",
                        "size": 5200000000u64,
                        "digest": "abc123",
                        "details": { "format": "gguf", "family": "qwen3" }
                    },
                    {
                        "name": "gemma4",
                        "model": "gemma4",
                        "modified_at": "2025-10-03T23:34:03.409490317-07:00",
                        "size": 9608350245u64,
                        "digest": "def456",
                        "details": { "format": "gguf", "family": "gemma4" }
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut prof = p("draft", AiProvider::Ollama, "unused");
        prof.base_url = Some(server.uri());
        prof.key = None;

        let models = listing_registry().list_models(&prof).await.unwrap();
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["gemma4", "qwen3:8b"]);
        assert_eq!(
            models[1].label.as_deref(),
            Some("qwen3:8b — 5200000000 bytes")
        );
    }

    #[tokio::test]
    async fn model_listing_is_sorted_and_truncated_to_two_thousand() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        let data: Vec<_> = (0..2005)
            .rev()
            .map(|n| {
                serde_json::json!({
                    "id": format!("model-{n:04}"),
                    "object": "model",
                    "created": 1686935002,
                    "owned_by": "test"
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": data
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut prof = p("draft", AiProvider::OpenAiCompatible, "unused");
        prof.base_url = Some(server.uri());

        let models = listing_registry().list_models(&prof).await.unwrap();
        assert_eq!(models.len(), MAX_LISTED_MODELS);
        assert_eq!(models.first().unwrap().id, "model-0000");
        assert_eq!(models.last().unwrap().id, "model-1999");
        assert!(models.windows(2).all(|pair| pair[0].id < pair[1].id));
    }

    #[tokio::test]
    async fn chunked_oversized_model_listing_is_rejected_at_the_byte_ceiling() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        crate::install_crypto_provider();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _bytes_read = socket.read(&mut request).await.unwrap();
            if socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .is_err()
            {
                return;
            }

            let chunk = [b' '; 16 * 1024];
            let mut remaining = usize::try_from(MAX_MODEL_LIST_BYTES).unwrap() + 1;
            while remaining > 0 {
                let len = remaining.min(chunk.len());
                if socket
                    .write_all(format!("{len:X}\r\n").as_bytes())
                    .await
                    .is_err()
                    || socket.write_all(&chunk[..len]).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
                remaining -= len;
            }
            let _write_result = socket.write_all(b"0\r\n\r\n").await;
        });
        let mut prof = p("draft", AiProvider::OpenAiCompatible, "unused");
        prof.base_url = Some(format!("http://{address}"));

        let error = listing_registry().list_models(&prof).await.unwrap_err();
        assert_eq!(error, ModelListingError::ResponseTooLarge);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn listing_errors_and_logs_exclude_keys_and_provider_bodies() {
        use tracing_subscriber::prelude::*;

        crate::install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("UPSTREAM-PRIVATE-BODY: alt-key must never be repeated"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut prof = p("draft", AiProvider::OpenAi, "unused");
        prof.base_url = Some(server.uri());
        let ring = Arc::new(crate::util::log_ring::LogRing::new(10));
        let subscriber = tracing_subscriber::registry()
            .with(crate::util::log_ring::LogRingLayer::new(ring.clone()));
        let _default = tracing::subscriber::set_default(subscriber);

        let error = listing_registry().list_models(&prof).await.unwrap_err();
        assert_eq!(error, ModelListingError::HttpStatus(401));
        assert!(!format!("{error:?}").contains("alt-key"));
        assert!(!format!("{error:?}").contains("UPSTREAM-PRIVATE-BODY"));

        let entries: Vec<_> = ring
            .snapshot()
            .into_iter()
            .filter(|entry| entry.message == "AI model listing failed")
            .collect();
        assert_eq!(entries.len(), 1, "one click must produce one warning");
        assert_eq!(entries[0].level, tracing::Level::WARN);
        assert!(
            entries[0]
                .fields
                .iter()
                .all(|(name, _)| matches!(name.as_str(), "kind" | "status"))
        );
        let logs = format!("{entries:?}");
        assert!(!logs.contains("alt-key"), "{logs}");
        assert!(!logs.contains("UPSTREAM-PRIVATE-BODY"), "{logs}");
    }
}
