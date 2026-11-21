use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{debug, error};

use llm::{ToolCall, ToolResultMessage, TraceIds};

use super::cache::TtlCache;
use super::client::{FetchError, SearchClient, SearchError, SearchResult, is_flake};
use super::media::{MediaClient, MediaError};
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
    /// Built per call rather than at startup so `ai.media.profile` edits
    /// apply live, the way every other settings knob does. `reqwest::Client`
    /// is an `Arc` internally, so the clone is cheap.
    fn media_client(&self) -> Option<MediaClient> {
        let media = self.live_media();
        let (_, profile) = self.registry.resolve_named(&media.profile)?;
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
            profile.provider.as_str(),
            profile.name.clone(),
            base_url,
            self.registry.secret_for(&profile),
            profile.model.clone(),
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
                let code = match &err {
                    SearchError::Timeout(_) => {
                        crate::log_handled!(warn, err, query, "web_search timed out");
                        "search_timeout"
                    }
                    SearchError::Upstream(e) if is_flake(e) => {
                        crate::log_handled!(warn, err, query, "web_search upstream flake");
                        "search_failed"
                    }
                    SearchError::Upstream(_) => {
                        crate::log_handled!(error, err, query, "web_search failed");
                        "search_failed"
                    }
                };
                json!({ "error": code }).to_string()
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

    /// Rejections about the model's own URL carry `details`; operational
    /// failures get only the code (ADR-0012 rule 3). The blocked target,
    /// often a resolved internal IP, stays in the log.
    fn map_fetch_err(err: &FetchError, url: &str) -> String {
        match err {
            FetchError::InvalidUrl(_) | FetchError::UnsupportedScheme(_) => {
                debug!(error = ?err, url, "read_url rejected the URL");
                json!({
                    "error": "invalid_url",
                    "details": "url must be an absolute http(s) URL",
                })
            }
            FetchError::Blocked { .. } => {
                debug!(error = ?err, url, "read_url blocked");
                json!({ "error": "fetch_blocked" })
            }
            FetchError::TooLarge => {
                debug!(error = ?err, url, "read_url payload too large");
                json!({ "error": "payload_too_large" })
            }
            FetchError::UnsupportedContentType(_) => {
                debug!(error = ?err, url, "read_url unsupported content type");
                json!({ "error": "unsupported_content_type" })
            }
            FetchError::NoReadableContent => {
                debug!(error = ?err, url, "read_url found no readable content");
                json!({ "error": "no_readable_content" })
            }
            FetchError::Timeout(_) => {
                crate::log_handled!(warn, err, url, "read_url timed out");
                json!({ "error": "fetch_timeout" })
            }
            FetchError::Status { status, source } => {
                if is_flake(source) {
                    crate::log_handled!(warn, err, url, "read_url upstream flake");
                } else {
                    debug!(error = ?err, url, "read_url target returned an error status");
                }
                json!({ "error": "fetch_failed", "status": status.as_u16() })
            }
            FetchError::Upstream(_) => {
                crate::log_handled!(error, err, url, "read_url fetch failed");
                json!({ "error": "fetch_failed" })
            }
        }
        .to_string()
    }

    fn map_media_err(err: &MediaError, url: &str) -> String {
        let code = match err {
            MediaError::Timeout(_) => {
                crate::log_handled!(warn, err, url, "read_url media timeout");
                "analysis_timeout"
            }
            MediaError::Upstream(e) if is_flake(e) => {
                crate::log_handled!(warn, err, url, "read_url media upstream flake");
                "analysis_failed"
            }
            MediaError::Upstream(_) | MediaError::EmptyAnswer => {
                crate::log_handled!(error, err, url, "read_url media failed");
                "analysis_failed"
            }
        };
        json!({ "error": code }).to_string()
    }
}

fn normalize_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cache key for a `read_url` target. Only the scheme and host are case
/// insensitive (`url::Url` lowercases both while parsing); path and query
/// stay as given, since the cache is shared across users and `/A` and `/a`
/// can be different documents. Unparseable input is keyed verbatim.
fn normalize_url(url: &str) -> String {
    let url = url.trim();
    url::Url::parse(url).map_or_else(|_| url.to_string(), String::from)
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
            ..Default::default()
        }
    }

    fn handle_with(profiles: Vec<AiProfile>, aurora: &str) -> SettingsHandle {
        let mut s = crate::settings::Settings::compiled_defaults();
        s.ai.profiles = profiles;
        s.ai.aurora.profile = aurora.to_string();
        crate::settings::SettingsHandle::from_settings(s)
    }

    #[test]
    fn media_client_follows_the_media_profile_and_live_profile_edits() {
        crate::install_crypto_provider();
        let handle = handle_with(
            vec![
                profile("vision", AiProvider::OpenRouter, "vision-model"),
                profile("chat", AiProvider::OpenRouter, "chat-model"),
            ],
            "chat",
        );
        {
            let mut s = (*handle.load()).clone();
            s.ai.media.profile = "vision".into();
            handle.store(Arc::new(s));
        }
        let ex = test_executor(handle.clone());
        let mc = ex.media_client().unwrap();
        assert_eq!(mc.model(), "vision-model");
        assert_eq!(mc.base_url(), "https://example.test/v1");

        // A live edit is picked up without rebuilding the executor.
        let mut s = (*handle.load()).clone();
        s.ai.profiles
            .iter_mut()
            .find(|p| p.name == "vision")
            .unwrap()
            .model = "second-model".into();
        handle.store(Arc::new(s));
        assert_eq!(ex.media_client().unwrap().model(), "second-model");
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

    /// The SSRF guard's target (here an internal IP) stays in the log; the
    /// model, which speaks in public chat, gets the bare code.
    #[tokio::test]
    async fn read_url_to_blocked_ip_returns_bare_code() {
        crate::ai::content::client::ssrf_bypass_for_tests(false);
        let executor = test_executor(test_handle());
        let call = ToolCall {
            id: "c1".into(),
            name: "read_url".into(),
            arguments: serde_json::json!({ "url": "http://10.1.2.3:8080/admin" }),
            arguments_parse_error: None,
        };
        let result = executor
            .execute_tool_call(&call, &TraceIds::default())
            .await;
        assert_eq!(result.content, r#"{"error":"fetch_blocked"}"#);
    }

    #[test]
    fn read_url_cache_key_folds_host_case_but_keeps_path_case() {
        assert_ne!(
            normalize_url("https://example.com/Doc?Id=A"),
            normalize_url("https://example.com/doc?id=a")
        );
        assert_eq!(
            normalize_url(" HTTPS://Example.COM/Doc?Id=A "),
            normalize_url("https://example.com/Doc?Id=A")
        );
    }
}
