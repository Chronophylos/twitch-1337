mod context;
mod generator;
mod queue;
mod scheduler;

use std::sync::Arc;

use eyre::Result;
use jiff::SignedDuration;
use jiff::civil::Time;
use tracing::{Instrument as _, info, info_span, warn};

use crate::emote_tracker::{EmoteTrackerHandle, PostWindowFilter};
use crate::lifecycle::Lifecycle;
use crate::llm_factory::LlmRegistry;
use crate::settings::SettingsHandle;
use crate::util::clock::Clock;
use crate::util::to_berlin;

pub use queue::EmotesAgentHandle;
pub use scheduler::EmoteWork;

const DISABLED_RECHECK_SECS: i64 = 300;
const THIRTY_DAYS_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Background worker for on-add, manual and nightly generation. All expensive
/// work happens here; producers only use the bounded [`EmotesAgentHandle`].
pub struct EmotesAgent {
    handle: EmotesAgentHandle,
    generator: generator::Generator,
    tracker: Arc<EmoteTrackerHandle>,
    settings: SettingsHandle,
    clock: Arc<dyn Clock>,
    lifecycle: Lifecycle,
}

impl EmotesAgent {
    #[expect(
        clippy::too_many_arguments,
        reason = "composition root wires shared agent dependencies once"
    )]
    pub fn new(
        handle: EmotesAgentHandle,
        registry: Arc<LlmRegistry>,
        settings: SettingsHandle,
        tracker: Arc<EmoteTrackerHandle>,
        clock: Arc<dyn Clock>,
        transcripts_dir: std::path::PathBuf,
        http: reqwest::Client,
        cdn_http: reqwest::Client,
        lifecycle: Lifecycle,
    ) -> Self {
        let generator = generator::Generator::new(
            registry,
            settings.clone(),
            tracker.clone(),
            clock.clone(),
            transcripts_dir,
            http,
            cdn_http,
        );
        Self {
            handle,
            generator,
            tracker,
            settings,
            clock,
            lifecycle,
        }
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn run(self) {
        let mut added = self.tracker.subscribe_added();
        let mut settings_changes = self.settings.subscribe();
        loop {
            if self.lifecycle.is_cancelled() {
                return;
            }
            let snapshot = self.settings.load();
            let enabled = snapshot.ai.emotes_agent.enabled;
            let run_at = snapshot.ai.emotes_agent.run_at.clone();
            drop(snapshot);
            let now = self.clock.now_utc();
            let target = if enabled {
                match Time::strptime("%H:%M", &run_at) {
                    Ok(time) => scheduler::next_run_at(to_berlin(now), time),
                    Err(error) => {
                        warn!(?error, %run_at, "invalid ai.emotes_agent.run_at; Emotes agent paused");
                        None
                    }
                }
            } else {
                None
            };
            let wake_at = target.unwrap_or(now + SignedDuration::from_secs(DISABLED_RECHECK_SECS));

            enum Wake {
                Added(crate::emote_tracker::EmoteAdded),
                Request(queue::EmoteAgentRequest),
                Nightly,
                Settings,
                Closed,
            }
            let wake = tokio::select! {
                biased;
                _ = self.lifecycle.wait_for_cancellation() => return,
                _ = settings_changes.next() => Wake::Settings,
                notice = added.recv() => match notice {
                    Ok(notice) => Wake::Added(notice),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        crate::util::observability::metrics::broadcast_lagged("emotes_agent", skipped);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => Wake::Closed,
                },
                request = self.handle.recv() => request.map_or(Wake::Closed, Wake::Request),
                _ = self.clock.sleep_until(wake_at) => Wake::Nightly,
            };
            if self.lifecycle.is_cancelled() {
                return;
            }
            match wake {
                Wake::Added(notice) => {
                    self.generate_logged(EmoteWork {
                        emote_id: notice.emote_id,
                        name: notice.name,
                    })
                    .await;
                }
                Wake::Request(queue::EmoteAgentRequest::Emote { emote_id, name }) => {
                    self.generate_logged(EmoteWork { emote_id, name }).await;
                }
                Wake::Request(queue::EmoteAgentRequest::RunNow) | Wake::Nightly => {
                    async {
                        if let Err(error) = self.run_nightly_batch().await {
                            crate::log_handled!(warn, error, "Emotes nightly batch failed");
                        }
                    }
                    .instrument(info_span!(parent: None, "emotes.batch", ai.agent = "emotes"))
                    .await;
                }
                Wake::Settings => {}
                Wake::Closed => return,
            }
        }
    }

    async fn generate_logged(&self, work: EmoteWork) {
        let id = work.emote_id.clone();
        async {
            if let Err(error) = self.generator.generate(&work.emote_id, &work.name).await {
                crate::log_handled!(warn, error, emote_id = %work.emote_id, "Emotes generation failed");
            }
        }
        .instrument(info_span!(
            parent: None,
            "emotes.generate",
            ai.agent = "emotes",
            emote.id = %id,
        ))
        .await;
    }

    /// Run one live-configured, capped priority batch. Individual generation
    /// failures leave their rows unchanged and do not abort the rest.
    pub async fn run_nightly_batch(&self) -> Result<usize> {
        let snapshot = self.settings.load();
        let config = snapshot.ai.emotes_agent.clone();
        let deny_logins = snapshot.twitch.emote_usage_deny_list.clone();
        drop(snapshot);
        if !config.enabled || self.lifecycle.is_cancelled() {
            return Ok(0);
        }
        let now = self.clock.now_utc().as_millisecond();
        let filter = PostWindowFilter {
            since_ms: now.saturating_sub(THIRTY_DAYS_MS),
            tryout_ms: i64::from(config.tryout_hours).saturating_mul(3_600_000),
            deny_logins,
        };
        let active = self.tracker.list_active_emotes().await?;
        let meanings = self.tracker.list_all_emote_meanings().await?;
        let counts = self.tracker.post_window_usage_counts(filter).await?;
        let globals = match self.generator.global_emotes().await {
            Ok(globals) => globals,
            Err(error) => {
                warn!(
                    ?error,
                    "Emotes global set fetch failed; skipping global tier"
                );
                Vec::new()
            }
        };
        let work = scheduler::select_batch(
            &active,
            &meanings,
            &counts,
            &globals,
            now,
            config.min_usages,
            config.batch_size,
        );
        let mut generated = 0;
        for item in work {
            if self.lifecycle.is_cancelled() || !self.settings.load().ai.emotes_agent.enabled {
                break;
            }
            match self.generator.generate(&item.emote_id, &item.name).await {
                Ok(true) => generated += 1,
                Ok(false) => {}
                Err(error) => {
                    crate::log_handled!(warn, error, emote_id = %item.emote_id, "Emotes batch item failed");
                }
            }
        }
        if generated > 0 {
            info!(generated, "Emotes meanings generated");
        }
        Ok(generated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use jiff::Timestamp;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Clone)]
    struct FixedClock(Timestamp);

    #[async_trait]
    impl Clock for FixedClock {
        fn now_utc(&self) -> Timestamp {
            self.0
        }
        async fn sleep_until(&self, target: Timestamp) {
            let delay = std::time::Duration::try_from(target - Timestamp::now())
                .unwrap_or(std::time::Duration::ZERO);
            tokio::time::sleep(delay).await;
        }
    }

    #[tokio::test]
    async fn nightly_batch_uses_seeded_db_fake_clock_and_wiremock_vision() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(EmoteTrackerHandle::open(&dir.path().join("emotes.db")).unwrap());
        tracker
            .upsert_emote("new", "NewEmote", true, 1)
            .await
            .unwrap();
        let mut settings = crate::settings::Settings::compiled_defaults();
        settings.ai.emotes_agent.enabled = true;
        settings.ai.emotes_agent.batch_size = 1;
        settings.ai.emotes = Some(crate::settings::AiEmotes {
            base_url: Some(server.uri()),
            ..Default::default()
        });
        settings.ai.profiles[0].provider = crate::settings::AiProvider::Ollama;
        settings.ai.profiles[0].base_url = Some(format!("{}/v1", server.uri()));
        settings.ai.profiles[0].model = "vision-test".into();
        let settings = SettingsHandle::from_settings(settings);
        let fixed = Timestamp::from_millisecond(50 * 86_400_000).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(FixedClock(fixed));
        let lifecycle = Lifecycle::new();
        let mut agent = EmotesAgent::new(
            EmotesAgentHandle::default(),
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
            lifecycle,
        );
        agent.generator.cdn_base = format!("{}/emote", server.uri());

        Mock::given(method("GET"))
            .and(path("/emote-sets/global"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"emotes": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/emote/new/2x_static.webp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/webp")
                    .set_body_bytes(b"webp"),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"appearance\":\"neu\",\"meaning\":null,\"usage\":null,\"avoid\":null}"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(agent.run_nightly_batch().await.unwrap(), 1);
        let row = tracker.get_emote_meaning("new").await.unwrap().unwrap();
        assert_eq!(row.appearance, "neu");
        assert_eq!(row.generated_at, Some(fixed.as_millisecond()));
    }

    #[tokio::test]
    async fn global_fetch_failure_does_not_abort_channel_generation() {
        crate::install_crypto_provider();
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(EmoteTrackerHandle::open(&dir.path().join("emotes.db")).unwrap());
        tracker
            .upsert_emote("new", "NewEmote", true, 1)
            .await
            .unwrap();
        let mut settings = crate::settings::Settings::compiled_defaults();
        settings.ai.emotes_agent.enabled = true;
        settings.ai.emotes = Some(crate::settings::AiEmotes {
            base_url: Some(server.uri()),
            ..Default::default()
        });
        settings.ai.profiles[0].provider = crate::settings::AiProvider::Ollama;
        settings.ai.profiles[0].base_url = Some(format!("{}/v1", server.uri()));
        settings.ai.profiles[0].model = "vision-test".into();
        let settings = SettingsHandle::from_settings(settings);
        let fixed = Timestamp::from_millisecond(50 * 86_400_000).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(FixedClock(fixed));
        let lifecycle = Lifecycle::new();
        let mut agent = EmotesAgent::new(
            EmotesAgentHandle::default(),
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
            lifecycle,
        );
        agent.generator.cdn_base = format!("{}/emote", server.uri());

        Mock::given(method("GET"))
            .and(path("/emote-sets/global"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/emote/new/2x_static.webp"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/webp")
                    .set_body_bytes(b"webp"),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"appearance\":\"neu\",\"meaning\":null,\"usage\":null,\"avoid\":null}"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(agent.run_nightly_batch().await.unwrap(), 1);
        assert_eq!(
            tracker
                .get_emote_meaning("new")
                .await
                .unwrap()
                .unwrap()
                .appearance,
            "neu"
        );
    }
}
