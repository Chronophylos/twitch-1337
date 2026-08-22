use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{error, warn};

use llm::{ToolCall, ToolResultMessage, TraceIds};

use super::cache::TtlCache;
use super::client::{SearchClient, SearchResult};
use super::media::MediaClient;
use crate::llm_factory::LlmRegistry;
use crate::settings::SettingsHandle;
use crate::settings::ai::AiMedia;

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct ReadUrlArgs {
    /// HTTP(S)-URL, die geöffnet werden soll.
    pub url: String,
    /// Optionale Frage zum Inhalt. Ohne diese kommt eine allgemeine
    /// Beschreibung zurück.
    #[serde(default)]
    pub instruction: Option<String>,
}

pub struct ContentToolExecutor {
    client: SearchClient,
    http: reqwest::Client,
    registry: Arc<LlmRegistry>,
    settings: SettingsHandle,
    search_cache: Arc<Mutex<TtlCache<Vec<SearchResult>>>>,
    read_cache: Arc<Mutex<TtlCache<ReadCacheEntry>>>,
}

#[derive(Debug, Clone)]
struct ReadCacheEntry {
    content_type: String,
    answer: String,
}

impl ContentToolExecutor {
    pub fn new(
        client: SearchClient,
        http: reqwest::Client,
        registry: Arc<LlmRegistry>,
        settings: SettingsHandle,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            client,
            http,
            registry,
            settings,
            search_cache: Arc::new(Mutex::new(TtlCache::new(cache_ttl, cache_capacity))),
            read_cache: Arc::new(Mutex::new(TtlCache::new(cache_ttl, cache_capacity))),
        }
    }

    fn live_media(&self) -> AiMedia {
        self.settings.load().ai.media.clone()
    }

    /// Build a media client for this call.
    ///
    /// Built per call rather than at startup so `ai.media.profile` and
    /// `ai.media.model` edits apply live, the way every other settings knob
    /// does. `reqwest::Client` is an `Arc` internally, so the clone is cheap.
    fn media_client(&self) -> Option<MediaClient> {
        let media = self.live_media();
        let (_, profile) = self.registry.resolve(media.profile.as_deref())?;
        let base_url = profile.base_url.clone().or_else(|| {
            profile
                .provider
                .openai_compat_base_url()
                .map(str::to_string)
        })?;
        // NOT `caps().default_base_url` — that is the *native* client's host shape.
        // `MediaClient` posts to `{base}/chat/completions`, so Ollama needs the `/v1`
        // shim path. Getting this wrong 404s every media call on an Ollama profile.
        Some(MediaClient::new(
            self.http.clone(),
            base_url,
            self.registry.secret_for(&profile),
            media.model.clone(),
            Duration::from_secs(media.timeout),
        ))
    }

    fn current_max_results(&self) -> usize {
        self.settings
            .load()
            .ai
            .web
            .as_ref()
            .map(|w| w.max_results)
            .unwrap_or(5)
    }

    pub fn max_results(&self) -> usize {
        self.current_max_results()
    }

    pub async fn execute_tool_call(&self, call: &ToolCall, trace: &TraceIds) -> ToolResultMessage {
        let content = self.execute(call, trace).await;
        ToolResultMessage::for_call(call, content)
    }

    async fn execute(&self, call: &ToolCall, trace: &TraceIds) -> String {
        match call.name.as_str() {
            "web_search" => match call.parse_args::<WebSearchArgs>() {
                Ok(args) => self.execute_web_search(args).await,
                Err(e) => Self::args_error_payload(&call.name, &e),
            },
            "read_url" => match call.parse_args::<ReadUrlArgs>() {
                Ok(args) => self.execute_read_url(args, trace).await,
                Err(e) => Self::args_error_payload(&call.name, &e),
            },
            other => json!({
                "error": "unknown_tool",
                "tool": other,
            })
            .to_string(),
        }
    }

    fn args_error_payload(tool: &str, err: &llm::ToolArgsError) -> String {
        match err {
            llm::ToolArgsError::Provider { error, raw } => json!({
                "error": "invalid_arguments_json",
                "tool": tool,
                "details": error,
                "raw": raw,
            })
            .to_string(),
            llm::ToolArgsError::Deserialize { error } => json!({
                "error": "invalid_arguments",
                "tool": tool,
                "details": error,
            })
            .to_string(),
        }
    }

    async fn execute_web_search(&self, args: WebSearchArgs) -> String {
        let query = args.query.trim();
        if query.is_empty() {
            return json!({
                "error": "invalid_arguments",
                "details": "query cannot be empty",
            })
            .to_string();
        }

        let live_max = self.current_max_results();
        let requested = args.max_results.unwrap_or(live_max);
        let effective_max = requested.clamp(1, live_max);

        let key = format!("{}::{}", normalize_query(query), effective_max);
        if let Some(cached) = self.search_cache.lock().await.get(&key) {
            return json!({
                "cached": true,
                "results": cached,
            })
            .to_string();
        }

        match self.client.web_search(query, effective_max).await {
            Ok(results) => {
                self.search_cache.lock().await.insert(key, results.clone());
                json!({
                    "cached": false,
                    "results": results,
                })
                .to_string()
            }
            Err(err) => {
                let error_code = if err
                    .chain()
                    .any(|cause| cause.to_string().to_ascii_lowercase().contains("timed out"))
                {
                    warn!(error = ?err, query, "web_search timed out");
                    "search_timeout"
                } else {
                    error!(error = ?err, query, "web_search failed");
                    "search_failed"
                };
                json!({
                    "error": error_code,
                    "details": format!("{err:#}"),
                })
                .to_string()
            }
        }
    }

    async fn execute_read_url(&self, args: ReadUrlArgs, trace: &TraceIds) -> String {
        let instruction = args
            .instruction
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let cache_key = format!(
            "{}::{}",
            normalize_url(&args.url),
            instruction.as_deref().unwrap_or("").to_ascii_lowercase()
        );
        if let Some(cached) = self.read_cache.lock().await.get(&cache_key) {
            return json!({
                "cached": true,
                "url": args.url,
                "content_type": cached.content_type,
                "answer": cached.answer,
            })
            .to_string();
        }

        let media_caps = self.live_media();
        let fetched = match self.client.fetch_for_read(&args.url, &media_caps).await {
            Ok(f) => f,
            Err(err) => return Self::map_fetch_err(&err, &args.url),
        };

        let Some(media) = self.media_client() else {
            error!(
                url = %args.url,
                "read_url media analysis unavailable: media profile could not resolve a client"
            );
            return json!({
                "error": "media_unavailable",
                "details": "media analysis is not configured or its profile is unavailable",
            })
            .to_string();
        };

        let answer = match media
            .analyze(
                &fetched.content_type,
                &fetched.payload,
                instruction.as_deref(),
                trace,
            )
            .await
        {
            Ok(a) => a,
            Err(err) => return Self::map_media_err(&err, &args.url),
        };

        self.read_cache.lock().await.insert(
            cache_key,
            ReadCacheEntry {
                content_type: fetched.content_type.clone(),
                answer: answer.clone(),
            },
        );

        json!({
            "cached": false,
            "url": args.url,
            "content_type": fetched.content_type,
            "answer": answer,
        })
        .to_string()
    }

    fn map_fetch_err(err: &eyre::Report, url: &str) -> String {
        let msg = err.to_string().to_ascii_lowercase();
        let code = if msg.contains("blocked") {
            warn!(error = ?err, url, "read_url blocked");
            "fetch_blocked"
        } else if msg.contains("too large") {
            warn!(error = ?err, url, "read_url payload too large");
            "payload_too_large"
        } else if msg.contains("unsupported") {
            warn!(error = ?err, url, "read_url unsupported content type");
            "unsupported_content_type"
        } else if msg.contains("timed out") {
            warn!(error = ?err, url, "read_url timed out");
            "fetch_timeout"
        } else {
            error!(error = ?err, url, "read_url fetch failed");
            "fetch_failed"
        };
        json!({
            "error": code,
            "details": format!("{err:#}"),
        })
        .to_string()
    }

    fn map_media_err(err: &eyre::Report, url: &str) -> String {
        let msg = err.to_string().to_ascii_lowercase();
        let code = if msg.contains("timed out") {
            warn!(error = ?err, url, "read_url media timeout");
            "analysis_timeout"
        } else {
            error!(error = ?err, url, "read_url media failed");
            "analysis_failed"
        };
        json!({
            "error": code,
            "details": format!("{err:#}"),
        })
        .to_string()
    }
}

fn normalize_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_url(url: &str) -> String {
    url.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use llm::{ChatCompletionRequest, LlmClient, ToolArgsError, ToolChatCompletionRequest};

    use std::time::Duration;

    use super::*;
    use crate::llm_factory::LlmRegistry;
    use crate::settings::AiProvider;
    use crate::settings::ai::AiProfile;
    use crate::settings::test_handle;

    /// Never actually invoked: `media_client()` only needs the profile that
    /// `LlmRegistry::resolve` returns alongside it, not a working client.
    struct DummyClient;

    #[async_trait::async_trait]
    impl LlmClient for DummyClient {
        async fn chat_completion(&self, _request: ChatCompletionRequest) -> llm::Result<String> {
            unimplemented!("DummyClient is never called in these tests")
        }

        async fn chat_completion_with_tools(
            &self,
            _request: ToolChatCompletionRequest,
        ) -> llm::Result<llm::ToolChatCompletionResponse> {
            unimplemented!("DummyClient is never called in these tests")
        }
    }

    fn registry(settings: SettingsHandle) -> Arc<LlmRegistry> {
        Arc::new(LlmRegistry::fixed(Arc::new(DummyClient), settings))
    }

    fn test_executor(settings: SettingsHandle) -> ContentToolExecutor {
        crate::install_crypto_provider();
        let client = SearchClient::new_with_client(
            "http://127.0.0.1:65535/search".to_string(),
            Duration::from_secs(1),
            reqwest::Client::new(),
        );
        ContentToolExecutor::new(
            client,
            reqwest::Client::new(),
            registry(settings.clone()),
            settings,
            Duration::from_secs(300),
            32,
        )
    }

    fn profile(name: &str, provider: AiProvider, model: &str) -> AiProfile {
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

    fn handle_with(profiles: Vec<AiProfile>, active: &str) -> SettingsHandle {
        let mut s = crate::settings::Settings::compiled_defaults();
        s.ai.profiles = profiles;
        s.ai.active_profile = active.to_string();
        crate::settings::SettingsHandle::new(arc_swap::ArcSwap::from_pointee(s))
    }

    #[test]
    fn media_client_follows_the_media_profile_and_live_model_edits() {
        crate::install_crypto_provider();
        let handle = handle_with(
            vec![
                profile("vision", AiProvider::OpenRouter, "vision-model"),
                profile("chat", AiProvider::OpenRouter, "chat-model"),
            ],
            "chat",
        );
        {
            let mut s = (*handle.load_full()).clone();
            s.ai.media.profile = Some("vision".into());
            s.ai.media.model = "override-model".into();
            handle.store(Arc::new(s));
        }
        let ex = test_executor(handle.clone());
        let mc = ex.media_client().unwrap();
        assert_eq!(mc.model(), "override-model");
        assert_eq!(mc.base_url(), "https://example.test/v1");

        // A live edit is picked up without rebuilding the executor.
        let mut s = (*handle.load_full()).clone();
        s.ai.media.model = "second-model".into();
        handle.store(Arc::new(s));
        assert_eq!(ex.media_client().unwrap().model(), "second-model");
    }

    #[test]
    fn max_results_reflects_live_settings() {
        use crate::settings::{Settings, ai::AiWeb as SettingsAiWeb};

        crate::install_crypto_provider();

        // Build a settings handle with web.max_results = 10.
        let mut base = Settings::compiled_defaults();
        base.ai.web = Some(SettingsAiWeb {
            base_url: "http://127.0.0.1:65535/search".to_string(),
            timeout: 1,
            max_results: 10,
            max_rounds: 3,
            cache_ttl_secs: 300,
            cache_capacity: 32,
        });
        let handle = crate::settings::SettingsHandle::new(arc_swap::ArcSwap::from_pointee(base));

        let client = SearchClient::new_with_client(
            "http://127.0.0.1:65535/search".to_string(),
            Duration::from_secs(1),
            reqwest::Client::new(),
        );
        let executor = ContentToolExecutor::new(
            client,
            reqwest::Client::new(),
            registry(handle.clone()),
            handle.clone(),
            Duration::from_secs(300),
            32,
        );

        assert_eq!(
            executor.max_results(),
            10,
            "initial max_results should be 10"
        );

        // Mutate the handle to max_results = 3.
        let mut updated = Settings::compiled_defaults();
        updated.ai.web = Some(SettingsAiWeb {
            base_url: "http://127.0.0.1:65535/search".to_string(),
            timeout: 1,
            max_results: 3,
            max_rounds: 3,
            cache_ttl_secs: 300,
            cache_capacity: 32,
        });
        handle.store(std::sync::Arc::new(updated));

        assert_eq!(
            executor.max_results(),
            3,
            "max_results should update to 3 after store"
        );
    }

    #[test]
    fn live_media_reflects_settings_update() {
        use crate::settings::Settings;
        use crate::settings::ai::AiMedia;
        use bytesize::ByteSize;

        crate::install_crypto_provider();

        let base = Settings::compiled_defaults();
        let handle = crate::settings::SettingsHandle::new(arc_swap::ArcSwap::from_pointee(base));

        let client = SearchClient::new_with_client(
            "http://127.0.0.1:65535/search".to_string(),
            Duration::from_secs(1),
            reqwest::Client::new(),
        );
        let executor = ContentToolExecutor::new(
            client,
            reqwest::Client::new(),
            registry(handle.clone()),
            handle.clone(),
            Duration::from_secs(300),
            32,
        );

        // Default image cap should be the compiled default.
        let default_cap = executor.live_media().max_image_size;

        // Mutate: set a tiny image cap.
        let mut updated = Settings::compiled_defaults();
        updated.ai.media = AiMedia {
            max_image_size: ByteSize::b(1),
            ..AiMedia::default()
        };
        handle.store(std::sync::Arc::new(updated));

        assert_eq!(
            executor.live_media().max_image_size,
            ByteSize::b(1),
            "live_media() must reflect updated cap; was {:?}",
            default_cap
        );
    }

    #[tokio::test]
    async fn rejects_unknown_tool_name() {
        let executor = test_executor(test_handle());
        let call = ToolCall {
            id: "c1".into(),
            name: "save_memory".into(),
            arguments: serde_json::json!({}),
            arguments_parse_error: None,
        };
        let result = executor
            .execute_tool_call(&call, &TraceIds::default())
            .await;
        assert!(
            result.content.contains("\"unknown_tool\""),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn read_url_surfaces_arguments_parse_error() {
        let executor = test_executor(test_handle());
        let call = ToolCall {
            id: "c1".into(),
            name: "read_url".into(),
            arguments: serde_json::Value::Null,
            arguments_parse_error: Some(ToolArgsError::Provider {
                error: "expected value".into(),
                raw: "{bad".into(),
            }),
        };
        let result = executor
            .execute_tool_call(&call, &TraceIds::default())
            .await;
        assert!(
            result.content.contains("\"invalid_arguments_json\""),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn read_url_returns_fetch_failed_for_unreachable_host() {
        let executor = test_executor(test_handle());
        let call = ToolCall {
            id: "c1".into(),
            name: "read_url".into(),
            arguments: serde_json::json!({ "url": "http://127.0.0.1:1/nope" }),
            arguments_parse_error: None,
        };
        let result = executor
            .execute_tool_call(&call, &TraceIds::default())
            .await;
        assert!(
            result.content.contains("\"fetch_blocked\"")
                || result.content.contains("\"fetch_failed\""),
            "{}",
            result.content
        );
    }
}
