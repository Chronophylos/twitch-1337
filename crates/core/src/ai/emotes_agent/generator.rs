use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, WrapErr as _, bail};
use jiff::civil::Time;
use llm::TraceIds;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use super::context::ContextSampler;
use crate::ai::content::client::Payload;
use crate::ai::content::media::MediaClient;
use crate::emote_tracker::{
    EmoteTrackerHandle, GeneratedMeaning, GlobalCatalogEmote, MeaningField, PostWindowFilter,
    fetch_global_set, validate_meaning_text,
};
use crate::llm_factory::LlmRegistry;
use crate::settings::SettingsHandle;
use crate::util::clock::Clock;
use crate::util::http::SendTraced as _;

const CONTEXT_LIMIT: u32 = 20;
const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_API_BASE: &str = "https://7tv.io/v3";
const DEFAULT_CDN_BASE: &str = "https://cdn.7tv.app/emote";
const THIRTY_DAYS_MS: i64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGenerated {
    appearance: String,
    meaning: Option<String>,
    usage: Option<String>,
    avoid: Option<String>,
}

/// Per-call vision generator. Both the selected profile and its endpoint are
/// resolved through `LlmRegistry` on every invocation, like the Media agent.
pub struct Generator {
    registry: Arc<LlmRegistry>,
    settings: SettingsHandle,
    tracker: Arc<EmoteTrackerHandle>,
    clock: Arc<dyn Clock>,
    sampler: ContextSampler,
    http: reqwest::Client,
    cdn_http: reqwest::Client,
    pub(super) cdn_base: String,
}

impl Generator {
    pub fn new(
        registry: Arc<LlmRegistry>,
        settings: SettingsHandle,
        tracker: Arc<EmoteTrackerHandle>,
        clock: Arc<dyn Clock>,
        transcripts_dir: std::path::PathBuf,
        http: reqwest::Client,
        cdn_http: reqwest::Client,
    ) -> Self {
        Self {
            registry,
            settings,
            tracker,
            clock,
            sampler: ContextSampler::new(transcripts_dir),
            http,
            cdn_http,
            cdn_base: DEFAULT_CDN_BASE.into(),
        }
    }

    pub async fn global_emotes(&self) -> Result<Vec<GlobalCatalogEmote>> {
        let snapshot = self.settings.load();
        let base = snapshot
            .ai
            .emotes
            .as_ref()
            .and_then(|cfg| cfg.base_url.as_deref())
            .unwrap_or(DEFAULT_API_BASE)
            .to_owned();
        drop(snapshot);
        fetch_global_set(&self.http, &base).await
    }

    /// Generate and atomically apply one row. `Ok(false)` covers disabled,
    /// fully curated, or invalid model output; invalid output is logged here
    /// exactly once and never reaches the database write.
    pub async fn generate(&self, emote_id: &str, name: &str) -> Result<bool> {
        let snapshot = self.settings.load();
        let config = snapshot.ai.emotes_agent.clone();
        let deny_logins = snapshot.twitch.emote_usage_deny_list.clone();
        let dreamer_run_at = if config.enabled && snapshot.ai.dreamer.enabled {
            Some(
                Time::strptime("%H:%M", &snapshot.ai.dreamer.run_at)
                    .wrap_err("invalid ai.dreamer.run_at")?,
            )
        } else {
            None
        };
        drop(snapshot);
        if !config.enabled {
            return Ok(false);
        }
        if !valid_emote_id(emote_id) {
            bail!("invalid 7TV emote id {emote_id:?}");
        }
        if self
            .tracker
            .get_emote_meaning(emote_id)
            .await?
            .is_some_and(|row| row.curated.is_all())
        {
            return Ok(false);
        }

        let now_ts = self.clock.now_utc();
        let now = now_ts.as_millisecond();
        let filter = PostWindowFilter {
            since_ms: now.saturating_sub(THIRTY_DAYS_MS),
            tryout_ms: i64::from(config.tryout_hours).saturating_mul(3_600_000),
            deny_logins,
        };
        let samples = self
            .tracker
            .sample_post_window_usage(emote_id, filter.clone(), CONTEXT_LIMIT)
            .await?;
        let context = self
            .sampler
            .transcript_lines(&samples, name, now_ts, dreamer_run_at)
            .await?;
        let usage_count = self
            .tracker
            .post_window_usage_count(emote_id, filter)
            .await?;
        let (content_type, image) = self.fetch_static_image(emote_id).await?;

        let Some((_client, profile)) = self.registry.resolve_named(&config.profile) else {
            bail!("no usable Chat profile named {:?}", config.profile);
        };
        let base_url = profile.base_url.clone().or_else(|| {
            profile
                .provider
                .openai_compat_base_url()
                .map(str::to_string)
        });
        let Some(base_url) = base_url else {
            bail!("Emotes profile {:?} has no base URL", profile.name);
        };
        let media = MediaClient::new(
            self.http.clone(),
            profile.provider.as_str(),
            profile.name.clone(),
            base_url,
            self.registry.secret_for(&profile),
            profile.model,
            Duration::from_secs(profile.timeout),
        );
        let context_lines = i64::try_from(context.len()).unwrap_or(i64::MAX);
        let input = json!({
            "emote_name": name,
            "post_window_usage_count_last_30_days": usage_count,
            "transcript_lines_newest_first": context,
        })
        .to_string();
        let answer = crate::util::observability::metrics::with_llm_scope(
            "emotes",
            &profile.name,
            media.analyze_with_system(
                &content_type,
                &Payload::Bytes(image),
                Some(&input),
                twitch_1337_data::prompts::EMOTES_AGENT,
                &TraceIds::default(),
            ),
        )
        .await
        .wrap_err("Emotes vision completion failed")?;

        let generated =
            match parse_generated(&answer, emote_id, name, now, context_lines, usage_count) {
                Ok(generated) => generated,
                Err(error) => {
                    warn!(
                        ?error,
                        emote_id, "Emotes agent returned invalid JSON meaning; row unchanged"
                    );
                    return Ok(false);
                }
            };
        self.tracker.apply_generated_meaning(generated).await?;
        Ok(true)
    }

    /// 7TV's v3 `host.files[].static_name` is `2x_static.webp` for animated
    /// WebPs. Static emotes instead expose `2x.webp`; the 404 fallback covers
    /// that case and yields the one-frame normal WebP.
    async fn fetch_static_image(&self, emote_id: &str) -> Result<(String, Vec<u8>)> {
        let base = format!("{}/{emote_id}", self.cdn_base.trim_end_matches('/'));
        if let Some(image) = self.fetch_image(&format!("{base}/2x_static.webp")).await? {
            return Ok(image);
        }
        self.fetch_image(&format!("{base}/2x.webp"))
            .await?
            .ok_or_else(|| eyre::eyre!("7TV CDN has no 2x WebP for emote {emote_id}"))
    }

    async fn fetch_image(&self, url: &str) -> Result<Option<(String, Vec<u8>)>> {
        let request_url = reqwest::Url::parse(url).wrap_err("invalid 7TV CDN URL")?;
        let response = self
            .cdn_http
            .get(request_url)
            .send_traced()
            .await
            .wrap_err_with(|| format!("fetch 7TV static frame {url}"))?;
        if response.status().is_redirection() {
            bail!("7TV CDN redirect refused");
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .wrap_err_with(|| format!("7TV CDN error for {url}"))?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("image/webp")
            .split(';')
            .next()
            .unwrap_or("image/webp")
            .to_owned();
        if !content_type.starts_with("image/") {
            bail!("7TV CDN returned non-image content type {content_type:?}");
        }
        let max_bytes = u64::try_from(MAX_IMAGE_BYTES).unwrap_or(u64::MAX);
        let bytes = crate::util::http::read_capped(response, max_bytes)
            .await
            .wrap_err("read 7TV static frame")?;
        Ok(Some((content_type, bytes)))
    }
}

fn valid_emote_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn parse_generated(
    answer: &str,
    emote_id: &str,
    name: &str,
    now: i64,
    context_lines: i64,
    usage_count: i64,
) -> Result<GeneratedMeaning> {
    let raw: RawGenerated = serde_json::from_str(answer.trim()).wrap_err("invalid Emotes JSON")?;
    let appearance = validate_meaning_text(MeaningField::Appearance, &raw.appearance)?
        .ok_or_else(|| eyre::eyre!("appearance must not be empty"))?;
    let meaning =
        validate_meaning_text(MeaningField::Meaning, raw.meaning.as_deref().unwrap_or(""))?;
    let usage = validate_meaning_text(MeaningField::Usage, raw.usage.as_deref().unwrap_or(""))?;
    let avoid = validate_meaning_text(MeaningField::Avoid, raw.avoid.as_deref().unwrap_or(""))?;
    Ok(GeneratedMeaning {
        emote_id: emote_id.into(),
        name: name.into(),
        appearance,
        meaning,
        usage,
        avoid,
        generated_at: now,
        context_lines,
        usage_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use async_trait::async_trait;
    use jiff::Timestamp;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Clone)]
    struct FixedClock(Timestamp);

    #[async_trait]
    impl Clock for FixedClock {
        fn now_utc(&self) -> Timestamp {
            self.0
        }
        async fn sleep_until(&self, target: Timestamp) {
            let delta = Duration::try_from(target - Timestamp::now()).unwrap_or(Duration::ZERO);
            tokio::time::sleep(delta).await;
        }
    }

    fn settings(server: &MockServer) -> SettingsHandle {
        let mut settings = Settings::compiled_defaults();
        settings.ai.emotes_agent.enabled = true;
        settings.ai.dreamer.run_at = "00:00".into();
        settings.ai.profiles[0].provider = crate::settings::AiProvider::Ollama;
        settings.ai.profiles[0].base_url = Some(format!("{}/v1", server.uri()));
        settings.ai.profiles[0].model = "vision-test".into();
        SettingsHandle::from_settings(settings)
    }

    async fn generator(
        server: &MockServer,
        dir: &tempfile::TempDir,
    ) -> (Generator, Arc<EmoteTrackerHandle>) {
        crate::install_crypto_provider();
        let settings = settings(server);
        let tracker = Arc::new(
            EmoteTrackerHandle::open(&dir.path().join("emotes.db")).expect("open test tracker"),
        );
        tracker
            .upsert_emote("emote1", "Kappa", true, 1)
            .await
            .expect("seed emote");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock(
            "2026-01-31T00:00:00Z".parse().expect("fixed time"),
        ));
        let mut generator = Generator::new(
            Arc::new(LlmRegistry::new(None, settings.clone())),
            settings,
            tracker.clone(),
            clock,
            dir.path().join("transcripts"),
            reqwest::Client::new(),
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("redirect-free CDN client"),
        );
        generator.cdn_base = format!("{}/emote", server.uri());
        (generator, tracker)
    }

    fn completion(answer: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": answer}}]
        }))
    }

    #[tokio::test]
    async fn generates_from_static_image_name_and_json() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (generator, tracker) = generator(&server, &dir).await;
        let used_at = "2026-01-30T00:00:01Z"
            .parse::<Timestamp>()
            .unwrap()
            .as_millisecond();
        tracker
            .insert_usage_events(vec![crate::emote_tracker::NewUsageEvent {
                at: used_at,
                emote_id: "emote1".into(),
                twitch_user_id: "42".into(),
                login: Some("alice".into()),
                flagged: false,
            }])
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("transcripts"))
            .await
            .unwrap();
        crate::util::persist::atomic_write_bytes_async(
            b"01:00:01  alice#42: Kappa im Witz\n",
            &dir.path().join("transcripts/2026-01-30.md"),
            "transcript",
        )
        .await
        .unwrap();
        Mock::given(method("GET"))
            .and(path("/emote/emote1/2x_static.webp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/webp")
                    .set_body_bytes(b"webp"),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains(
                "\\\"post_window_usage_count_last_30_days\\\":1",
            ))
            .and(body_string_contains("Kappa im Witz"))
            .and(body_string_contains("\"type\":\"image_url\""))
            .respond_with(completion(
                r#"{"appearance":"lachendes Gesicht","meaning":"etwas ist lustig","usage":"bei Witzen","avoid":"bei Trauer"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        assert!(generator.generate("emote1", "Kappa").await.unwrap());
        let row = tracker.get_emote_meaning("emote1").await.unwrap().unwrap();
        assert_eq!(row.appearance, "lachendes Gesicht");
        assert_eq!(row.name.as_deref(), Some("Kappa"));
        assert_eq!(row.context_lines, 1);
        assert_eq!(row.usage_count, 1);
        assert_eq!(row.generated_at, Some(1_769_817_600_000));
        assert_eq!(row.curated.bits(), 0);
    }

    #[tokio::test]
    async fn cdn_redirect_to_another_origin_is_refused() {
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (generator, _tracker) = generator(&source, &dir).await;
        Mock::given(method("GET"))
            .and(path("/redirect.webp"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/image.webp", target.uri())),
            )
            .mount(&source)
            .await;
        Mock::given(method("GET"))
            .and(path("/image.webp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/webp")
                    .set_body_bytes(b"webp"),
            )
            .expect(0)
            .mount(&target)
            .await;

        let result = generator
            .fetch_image(&format!("{}/redirect.webp", source.uri()))
            .await;
        assert!(result.is_err(), "cross-origin redirect must be refused");
    }

    #[tokio::test]
    async fn disabled_agent_does_no_io_and_writes_nothing() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (generator, tracker) = generator(&server, &dir).await;
        let mut settings = (*generator.settings.load()).clone();
        settings.ai.emotes_agent.enabled = false;
        generator.settings.store(Arc::new(settings));
        assert!(!generator.generate("emote1", "Kappa").await.unwrap());
        assert!(tracker.get_emote_meaning("emote1").await.unwrap().is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn invalid_output_leaves_existing_row_unchanged() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (generator, tracker) = generator(&server, &dir).await;
        tracker
            .upsert_emote_meaning("emote1", "alt", Some("bleibt"), None, None, 1)
            .await
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/emote/emote1/2x_static.webp"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/emote/emote1/2x.webp"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"webp"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(completion(
                r#"{"appearance":"","meaning":"neu","usage":"neu","avoid":"neu"}"#,
            ))
            .mount(&server)
            .await;

        assert!(!generator.generate("emote1", "Kappa").await.unwrap());
        let row = tracker.get_emote_meaning("emote1").await.unwrap().unwrap();
        assert_eq!(row.appearance, "alt");
        assert_eq!(row.meaning.as_deref(), Some("bleibt"));
        assert_eq!(row.generated_at, None);
    }

    #[test]
    fn validation_rejects_controls_and_long_fields() {
        let base = r#"{"appearance":"ok","meaning":"x","usage":"x","avoid":"x"}"#;
        assert!(parse_generated(base, "e", "n", 1, 0, 0).is_ok());
        let control = r#"{"appearance":"bad\nline","meaning":null,"usage":null,"avoid":null}"#;
        assert!(parse_generated(control, "e", "n", 1, 0, 0).is_err());
        let long = format!(
            "{{\"appearance\":\"{}\",\"meaning\":null,\"usage\":null,\"avoid\":null}}",
            "x".repeat(301)
        );
        assert!(parse_generated(&long, "e", "n", 1, 0, 0).is_err());
    }
}
