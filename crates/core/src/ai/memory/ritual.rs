//! Daily dreamer ritual. Spawned from `run_bot`; sleeps until [ai.dreamer].run_at,
//! rotates the transcript, and runs the dreamer LLM against every memory file
//! plus yesterday's transcript inside nonce-fenced blocks.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use jiff::civil::{Date, DateTime, Time};
use jiff::{SignedDuration, Timestamp, ZonedDateTime};
use llm::{
    AgentOpts, AgentOutcome, LlmClient, LlmError, Message, ToolCall, ToolChatCompletionRequest,
    ToolChatCompletionResponse, ToolExecutor, ToolResultMessage, run_agent,
};
use tracing::{error, info, warn};

use crate::ai::memory::inject::{
    BuildOpts, FenceLabel, InvocationChannel, build_chat_turn_context, fence_block, fresh_nonce,
    scrub_for_inject,
};
use crate::ai::memory::store::MemoryStore;
use crate::ai::memory::tools::{DreamerExecutor, DreamerExecutorOpts, dreamer_tools};
use crate::ai::memory::transcript::TranscriptWriter;
use crate::ai::memory::types::FileKind;
use crate::ai::preset::render_standalone;
use crate::lifecycle::Lifecycle;
use crate::llm_factory::LlmRegistry;
use crate::settings::SettingsHandle;
use crate::twitch::mouth::Agent;

/// Resolve `run_at` on `date` to a Berlin `ZonedDateTime`, bumping forward an
/// hour when the requested local time is not unambiguous: across the
/// spring-forward DST gap (02:00–03:00 on the last Sunday in March) or out of
/// the autumn fold. Returns `None` only if even the bumped candidate fails to
/// resolve, which should not happen for any real Berlin date.
fn resolve_berlin_run_at(date: Date, time: Time) -> Option<ZonedDateTime> {
    let dt: DateTime = date.to_datetime(time);
    if let Ok(resolved) = crate::util::BERLIN
        .to_ambiguous_zoned_datetime(dt)
        .unambiguous()
    {
        return Some(resolved);
    }
    // DST gap: bump forward an hour to land past the missing window.
    let bumped = dt + SignedDuration::from_hours(1);
    crate::util::BERLIN
        .to_ambiguous_zoned_datetime(bumped)
        .unambiguous()
        .ok()
}

pub fn spawn_ritual(
    registry: Arc<LlmRegistry>,
    store: MemoryStore,
    transcript: TranscriptWriter,
    settings: SettingsHandle,
    lifecycle: Lifecycle,
    langfuse: Option<Arc<crate::ai::langfuse::LangfuseHandle>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if lifecycle.is_cancelled() {
                return;
            }

            // Re-read settings on each loop iteration. This picks up live edits
            // to dreamer.{enabled, model, reasoning_effort, run_at, timeout_secs,
            // max_rounds, max_writes_per_turn} and memory.inject_byte_budget.
            let snap = settings.load();
            let ai = &snap.ai;
            if !ai.dreamer.enabled {
                // Sleep a fixed short interval and re-check; cheap way to support
                // toggling enabled on/off without restarting the bot. 5 minutes
                // matches the latency-monitor cadence and is well under any
                // realistic run_at granularity.
                drop(snap);
                tokio::select! {
                    biased;
                    _ = lifecycle.wait_for_cancellation() => return,
                    _ = tokio::time::sleep(Duration::from_secs(300)) => {}
                }
                continue;
            }
            let run_at = Time::strptime("%H:%M", &ai.dreamer.run_at)
                .expect("ai.dreamer.run_at validated at settings store");
            drop(snap); // do not hold a Guard across await

            let now = crate::util::to_berlin(Timestamp::now());
            let target = resolve_berlin_run_at(now.date(), run_at);
            let next = target.filter(|t| *t > now).unwrap_or_else(|| {
                let tomorrow = now.date().tomorrow().expect("valid next day");
                resolve_berlin_run_at(tomorrow, run_at)
                    .expect("post-gap fallback resolves on the next day")
            });
            let wait = std::time::Duration::try_from(next.timestamp() - now.timestamp())
                .unwrap_or_default();
            tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => return,
                _ = tokio::time::sleep(wait) => {}
            }

            // A wake-up can race cancellation. Do not enter the file/LLM
            // operation until the operation-boundary check immediately below.
            let yesterday = next.date().yesterday().expect("valid prev day");
            if let Err(e) = run_ritual_until(
                &registry,
                &store,
                &transcript,
                &settings,
                yesterday,
                &lifecycle,
                langfuse.as_ref(),
            )
            .await
            {
                warn!(error = ?e, "dreamer ritual failed");
            }
        }
    })
}

/// Adds a cancellation gate immediately before each provider request while
/// leaving an in-flight provider request to finish under its existing timeout.
struct LifecycleLlmClient<'a> {
    inner: &'a dyn LlmClient,
    lifecycle: &'a Lifecycle,
}

#[async_trait]
impl LlmClient for LifecycleLlmClient<'_> {
    async fn chat_completion(
        &self,
        request: llm::ChatCompletionRequest,
    ) -> std::result::Result<String, LlmError> {
        if self.lifecycle.is_cancelled() {
            return Err(self.cancelled_error());
        }
        self.inner.chat_completion(request).await
    }

    async fn chat_completion_with_tools(
        &self,
        request: ToolChatCompletionRequest,
    ) -> std::result::Result<ToolChatCompletionResponse, LlmError> {
        if self.lifecycle.is_cancelled() {
            return Err(self.cancelled_error());
        }
        self.inner.chat_completion_with_tools(request).await
    }
}

impl LifecycleLlmClient<'_> {
    fn cancelled_error(&self) -> LlmError {
        LlmError::Provider {
            status: 0,
            body: "dreamer ritual cancelled before provider request".into(),
        }
    }
}

/// Gates each tool call independently. The wrapped memory operation is atomic:
/// once it starts it may finish, but no later tool call starts after stop.
struct LifecycleToolExecutor<'a> {
    inner: &'a DreamerExecutor,
    lifecycle: &'a Lifecycle,
}

#[async_trait]
impl ToolExecutor for LifecycleToolExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        if self.lifecycle.is_cancelled() {
            return ToolResultMessage::for_call(call, "cancelled: dreamer ritual stopped");
        }
        self.inner.execute(call).await
    }
}

/// Twitch user ids the transcript itself carries (`HH:MM:SS  login#<id>: …`).
/// These come from IRC tags, so a sheet may be created for them even though no
/// file exists yet — unlike a bare login, which is not an id and never becomes
/// one by guessing.
fn transcript_user_ids(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            // Past the fixed-width "HH:MM:SS  " stamp, then up to the
            // login/message separator.
            let (head, _) = line.get(11..)?.split_once(": ")?;
            let (_, id) = head.rsplit_once('#')?;
            (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
        })
        .collect()
}

pub async fn run_ritual(
    registry: &LlmRegistry,
    store: &MemoryStore,
    transcript: &TranscriptWriter,
    settings: &SettingsHandle,
    rotate_to: Date,
    langfuse: Option<&Arc<crate::ai::langfuse::LangfuseHandle>>,
) -> Result<()> {
    run_ritual_until(
        registry,
        store,
        transcript,
        settings,
        rotate_to,
        &Lifecycle::new(),
        langfuse,
    )
    .await
}

async fn run_ritual_until(
    registry: &LlmRegistry,
    store: &MemoryStore,
    transcript: &TranscriptWriter,
    settings: &SettingsHandle,
    rotate_to: Date,
    lifecycle: &Lifecycle,
    langfuse: Option<&Arc<crate::ai::langfuse::LangfuseHandle>>,
) -> Result<()> {
    if lifecycle.is_cancelled() {
        return Ok(());
    }

    // Snapshot once at top; build local config from live settings values.
    let snap = settings.load();
    let ai = &snap.ai;
    let dreamer_profile = ai.dreamer.profile.clone();
    let timeout_secs = ai.dreamer.timeout_secs;
    let max_rounds = ai.dreamer.max_rounds;
    let max_writes_per_turn = ai.dreamer.max_writes_per_turn;
    let inject_byte_budget = ai.memory.inject_byte_budget;
    // Caps are dashboard-tunable, so the prompt has to carry the live numbers.
    // Hardcoding them in dreamer.md made every raise a no-op: the model kept
    // compressing to the stale figures.
    let caps_line = format!(
        "SOUL {} B, LORE {} B, user {} B, scratch {} B",
        ai.memory.soul_bytes, ai.memory.lore_bytes, ai.memory.user_bytes, ai.memory.scratch_bytes
    );
    drop(snap);

    // Rotate first, then bail: a misconfigured model must not leave today.md
    // growing forever without a date boundary. Rotation is one atomic file
    // operation: cancellation observed during it is handled immediately after.
    let dated = transcript.rotate_to(rotate_to).await?;
    if lifecycle.is_cancelled() {
        return Ok(());
    }
    // Resolve fresh on every run, not once at spawn, so a dashboard profile
    // switch (or an edit to ai.dreamer.profile) applies to the next run.
    let Some((llm, base_profile)) = registry.resolve_named(&dreamer_profile) else {
        warn!("dreamer skipped, no usable LLM profile");
        return Ok(());
    };
    let model = base_profile.model;
    let reasoning_effort = base_profile.reasoning_effort;
    let service_tier = base_profile.service_tier;
    if model.trim().is_empty() {
        warn!("dreamer skipped, AI model is empty in settings");
        return Ok(());
    }
    let transcript_text = tokio::fs::read_to_string(&dated).await.unwrap_or_default();
    let nonce = fresh_nonce();

    let mem_ctx = build_chat_turn_context(
        store,
        BuildOpts {
            inject_byte_budget,
            nonce: nonce.clone(),
            primary_history: None,
            primary_login: String::new(),
            ai_channel_history: None,
            ai_channel_login: None,
            invocation_channel: InvocationChannel::Primary,
            self_logins: Vec::new(),
            // Dreamer never renders recent_chat (no history buffers supplied),
            // so this is never read; the value is arbitrary.
            current_agent: Agent::Aurora,
            // Dreamer has no speaker. Empty string + no history buffers
            // tells build_chat_turn_context to skip the chat-window scope
            // filter so the dreamer still sees every user file in its
            // system prompt.
            speaker_login: String::new(),
            recap_sheet_logins: BTreeSet::new(),
            n: 200,
            ai_channel_n: 200,
            batch_size: 40,
            recap_through_seq: 0,
            ai_channel_recap_through_seq: 0,
            recap_enabled: false,
        },
    )
    .await?;
    if lifecycle.is_cancelled() {
        return Ok(());
    }
    let date_str = rotate_to.strftime("%Y-%m-%d").to_string();
    let transcript_block = fence_block(
        FenceLabel::Transcript { date: &date_str },
        &nonce,
        &scrub_for_inject(&transcript_text),
    );

    let now_str = crate::util::to_berlin(Timestamp::now())
        .strftime("%Y-%m-%d")
        .to_string();
    // Prompt template is baked into the binary (#321), not read from disk.
    let head = render_standalone(
        twitch_1337_data::prompts::DREAMER,
        minijinja::context! { date => &now_str, caps => &caps_line },
    )?;
    // The dreamer wants every file in one shot, so glue memory + scratch
    // blocks back together for its system prompt. The cache-hygiene
    // split only matters for the chat-turn loop.
    let mut mem_block = mem_ctx.memory;
    if !mem_ctx.scratch.is_empty() {
        if !mem_block.is_empty() {
            mem_block.push('\n');
        }
        mem_block.push_str(&mem_ctx.scratch);
    }
    if lifecycle.is_cancelled() {
        return Ok(());
    }
    // Roster of every user sheet, listed independently of the byte budget that
    // packed `mem_block`. A sheet dropped for space used to read as "this
    // regular has no file yet", and the dreamer answered that by inventing an
    // id for the login it saw in the transcript.
    let users = store.list_users().await?;
    if lifecycle.is_cancelled() {
        return Ok(());
    }
    let mut roster_lines: Vec<String> = Vec::with_capacity(users.len());
    let mut writable_ids: BTreeSet<String> = BTreeSet::new();
    for f in &users {
        let FileKind::User { user_id } = &f.kind else {
            continue;
        };
        writable_ids.insert(user_id.clone());
        match f.frontmatter.username.as_deref().filter(|l| !l.is_empty()) {
            Some(login) => roster_lines.push(format!("- users/{user_id}.md ({login})")),
            None => roster_lines.push(format!("- users/{user_id}.md")),
        }
    }
    // Ids seen in the transcript are real (IRC tags), so the dreamer may open a
    // sheet for a newcomer. Everything outside roster ∪ transcript is invented.
    writable_ids.extend(transcript_user_ids(&transcript_text));
    let roster = format!(
        "## User-Index\n\nGenau diese User-Dateien existieren. Für jemanden ohne Datei ist die id die Zahl hinter dem `#` im Transkript. Jede andere id lehnt `write_file` mit `unknown_user_id` ab.\n\n{}",
        roster_lines.join("\n")
    );
    let system_prompt = format!("{head}\n\n{roster}\n\n{mem_block}\n{transcript_block}");

    let exec = DreamerExecutor::new(DreamerExecutorOpts {
        store: store.clone(),
        max_writes_per_turn,
        known_user_ids: writable_ids,
    });

    let req = ToolChatCompletionRequest {
        model,
        messages: vec![Message::system(system_prompt), Message::user("revise.")],
        tools: dreamer_tools(),
        reasoning_effort,
        service_tier,
        prior_rounds: Vec::new(),
        trace: llm::TraceIds {
            user: Some("<dreamer>".to_string()),
            session_id: Some(crate::ai::session::new_session_id()),
        },
    };
    // Offline, owner-scheduled run with its own write cap: no per-round call
    // cap or whole-run deadline.
    let opts = AgentOpts {
        max_rounds,
        per_round_timeout: Some(Duration::from_secs(timeout_secs)),
        max_calls_per_round: None,
        deadline: None,
    };
    let session_id = req.trace.session_id.clone();
    llm::with_optional_turn(
        langfuse.map(crate::ai::langfuse::LangfuseHandle::as_observer),
        llm::TurnParams {
            name: "dreamer".into(),
            user: Some("<dreamer>".into()),
            session_id,
            tags: vec!["dreamer".into()],
        },
        async {
            if lifecycle.is_cancelled() {
                return;
            }
            let lifecycle_client = LifecycleLlmClient {
                inner: &*llm,
                lifecycle,
            };
            let lifecycle_executor = LifecycleToolExecutor {
                inner: &exec,
                lifecycle,
            };
            match run_agent(&lifecycle_client, req, &lifecycle_executor, opts).await {
                Ok(AgentOutcome::Text(_)) => {
                    info!(rotated = %dated.display(), "dreamer ritual finished");
                }
                Ok(AgentOutcome::MaxRoundsExceeded) => warn!("dreamer max_rounds reached"),
                Ok(AgentOutcome::Timeout { round } | AgentOutcome::DeadlineExceeded { round }) => {
                    warn!(round, "dreamer per-round timeout");
                }
                Err(_error) if lifecycle.is_cancelled() => {
                    info!("dreamer ritual cancelled at an operation boundary");
                }
                Err(error) => error!(error = ?error, "dreamer llm error"),
            }
        },
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_user_ids_reads_ids_and_ignores_bare_logins() {
        let text = "\
13:37:00  alice#123: hi
13:37:01  bob: no id here
13:37:02  aurora#456: yo: with a colon
not a transcript line
13:38:00  weird#abc: id must be numeric
";
        let ids = transcript_user_ids(text);
        assert_eq!(
            ids,
            BTreeSet::from(["123".to_string(), "456".to_string()]),
            "got {ids:?}"
        );
    }

    #[test]
    fn resolve_berlin_run_at_handles_dst_gap() {
        // 2026 spring-forward in Berlin is Sunday 2026-03-29; 02:00–03:00 local does not exist.
        let date = jiff::civil::date(2026, 3, 29);
        let in_gap = jiff::civil::time(2, 30, 0, 0);
        let resolved = resolve_berlin_run_at(date, in_gap).expect("falls through past gap");
        assert_eq!(resolved.hour(), 3, "should bump into the post-DST hour");

        let four_am = jiff::civil::time(4, 0, 0, 0);
        let resolved = resolve_berlin_run_at(date, four_am).expect("04:00 always resolves");
        assert_eq!(resolved.hour(), 4);
    }

    #[tokio::test]
    async fn cancellation_before_ritual_prevents_rotation_and_provider_work() {
        use async_trait::async_trait;
        use llm::{
            ChatCompletionRequest, LlmClient, LlmError, ToolChatCompletionRequest,
            ToolChatCompletionResponse,
        };

        struct UnexpectedLlm;
        #[async_trait]
        impl LlmClient for UnexpectedLlm {
            async fn chat_completion(
                &self,
                _request: ChatCompletionRequest,
            ) -> std::result::Result<String, LlmError> {
                panic!("cancelled ritual must not call the provider")
            }
            async fn chat_completion_with_tools(
                &self,
                _request: ToolChatCompletionRequest,
            ) -> Result<ToolChatCompletionResponse, LlmError> {
                panic!("cancelled ritual must not call the provider")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let settings =
            SettingsHandle::from_settings(crate::settings::Settings::compiled_defaults());
        let store = MemoryStore::open(dir.path(), settings.clone())
            .await
            .unwrap();
        let transcript = TranscriptWriter::open(store.memories_dir()).await.unwrap();
        let registry = LlmRegistry::fixed(
            Arc::new(UnexpectedLlm) as Arc<dyn LlmClient>,
            settings.clone(),
        );
        let lifecycle = Lifecycle::new();
        lifecycle.request_graceful_stop();
        let rotate_to = jiff::civil::date(2026, 4, 17);

        run_ritual_until(
            &registry,
            &store,
            &transcript,
            &settings,
            rotate_to,
            &lifecycle,
            None,
        )
        .await
        .unwrap();

        assert!(!transcript.dated_path_for(rotate_to).exists());
    }

    #[tokio::test]
    async fn cancellation_during_llm_request_finishes_it_but_blocks_the_next_write() {
        use async_trait::async_trait;
        use llm::{
            ChatCompletionRequest, LlmClient, LlmError, ToolCall, ToolChatCompletionRequest,
            ToolChatCompletionResponse,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        struct GatedLlm {
            started: Notify,
            release: Notify,
            calls: AtomicUsize,
        }

        #[async_trait]
        impl LlmClient for GatedLlm {
            async fn chat_completion(
                &self,
                _request: ChatCompletionRequest,
            ) -> std::result::Result<String, LlmError> {
                unreachable!("ritual uses the tool-calling client")
            }

            async fn chat_completion_with_tools(
                &self,
                _request: ToolChatCompletionRequest,
            ) -> std::result::Result<ToolChatCompletionResponse, LlmError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                self.release.notified().await;
                Ok(ToolChatCompletionResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "write-1".into(),
                        name: "write_file".into(),
                        arguments: serde_json::json!({
                            "path": "LORE.md",
                            "body": "must not be written after cancellation"
                        }),
                        arguments_parse_error: None,
                    }],
                    reasoning_content: None,
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut settings_value = crate::settings::Settings::compiled_defaults();
        settings_value.ai.profiles[0].model = "model-v1".into();
        let settings = SettingsHandle::from_settings(settings_value);
        let store = MemoryStore::open(dir.path(), settings.clone())
            .await
            .unwrap();
        store
            .write(
                &FileKind::Lore,
                "sentinel before ritual cancellation",
                crate::ai::memory::store::UserIdentity::default(),
            )
            .await
            .unwrap();
        let transcript = TranscriptWriter::open(store.memories_dir()).await.unwrap();
        let client = Arc::new(GatedLlm {
            started: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        });
        let registry = Arc::new(LlmRegistry::fixed(
            client.clone() as Arc<dyn LlmClient>,
            settings.clone(),
        ));
        let lifecycle = Lifecycle::new();
        let rotate_to = jiff::civil::date(2026, 4, 17);

        let task = {
            let registry = registry.clone();
            let store = store.clone();
            let transcript = transcript.clone();
            let settings = settings.clone();
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move {
                run_ritual_until(
                    &registry,
                    &store,
                    &transcript,
                    &settings,
                    rotate_to,
                    &lifecycle,
                    None,
                )
                .await
            })
        };

        client.started.notified().await;
        lifecycle.request_graceful_stop();
        client.release.notify_one();
        task.await.unwrap().unwrap();

        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.read_kind(&FileKind::Lore).await.unwrap().body,
            "sentinel before ritual cancellation",
            "a tool effect must not start after the active provider operation finishes"
        );
    }

    /// Demonstrates live config rebinding: `run_ritual` reads settings fresh on
    /// each call, so a model change stored in the `SettingsHandle` between two
    /// invocations is picked up on the second call without restarting the bot.
    ///
    /// This is a compile+logic unit test — it verifies that the model name
    /// passed to the LLM matches what is currently in the settings handle at
    /// the time `run_ritual` is called, not what was present at spawn time.
    #[tokio::test]
    async fn run_ritual_reads_settings_live() {
        use std::sync::Arc;
        use std::sync::Mutex;

        use async_trait::async_trait;
        use llm::{
            ChatCompletionRequest, LlmClient, LlmError, ToolChatCompletionRequest,
            ToolChatCompletionResponse,
        };
        use tempfile::TempDir;

        use crate::settings::{Settings, SettingsHandle};

        // ── Minimal fake LLM that records the model name from each tool call ──
        struct ModelRecorder(Mutex<Vec<String>>);
        #[async_trait]
        impl LlmClient for ModelRecorder {
            async fn chat_completion(&self, _req: ChatCompletionRequest) -> llm::Result<String> {
                Err(LlmError::Provider {
                    status: 0,
                    body: "not used".into(),
                })
            }
            async fn chat_completion_with_tools(
                &self,
                req: ToolChatCompletionRequest,
            ) -> llm::Result<ToolChatCompletionResponse> {
                self.0.lock().unwrap().push(req.model.clone());
                // Immediately terminate the agent loop.
                Ok(ToolChatCompletionResponse::Message("done".into()))
            }
        }

        // ── Wire up a temp data dir + store + transcript ──
        let dir = TempDir::new().unwrap();
        let settings: SettingsHandle = SettingsHandle::from_settings(Settings::compiled_defaults());
        let store = crate::ai::memory::store::MemoryStore::open(dir.path(), settings.clone())
            .await
            .unwrap();
        let transcript =
            crate::ai::memory::transcript::TranscriptWriter::open(store.memories_dir())
                .await
                .unwrap();

        // No prompt files are seeded to disk (#321) — `run_ritual` uses the
        // baked-in `dreamer.md` template directly, so it works with nothing
        // under `$DATA_DIR/prompts/`.

        let recorder = Arc::new(ModelRecorder(Mutex::new(Vec::new())));
        let yesterday = jiff::civil::date(2026, 4, 17);

        // ── First call: uses the profile's model ──
        // compiled_defaults has an empty profile model; set it explicitly.
        {
            let mut s = (*settings.load()).clone();
            s.ai.profiles[0].model = "model-v1".into();
            settings.store(Arc::new(s));
        }
        let registry = crate::llm_factory::LlmRegistry::fixed(
            recorder.clone() as Arc<dyn LlmClient>,
            settings.clone(),
        );
        run_ritual(&registry, &store, &transcript, &settings, yesterday, None)
            .await
            .unwrap();

        // ── Second call: change model in settings (simulates a live dashboard edit) ──
        {
            let mut s = (*settings.load()).clone();
            s.ai.profiles[0].model = "model-v2".into();
            settings.store(Arc::new(s));
        }
        // Use a different date so rotate_to doesn't conflict with the already-rotated file.
        let yesterday2 = jiff::civil::date(2026, 4, 16);
        run_ritual(&registry, &store, &transcript, &settings, yesterday2, None)
            .await
            .unwrap();

        let models = recorder.0.lock().unwrap().clone();
        assert_eq!(
            models.len(),
            2,
            "expected 2 LLM calls, got {}",
            models.len()
        );
        assert_eq!(models[0], "model-v1", "first call should use model-v1");
        assert_eq!(
            models[1], "model-v2",
            "second call should pick up live change to model-v2"
        );
    }

    /// The byte caps in `dreamer.md` used to be hardcoded text, so raising
    /// them in the dashboard did nothing and the dreamer kept compressing to
    /// the stale numbers. The prompt must carry the live values.
    #[tokio::test]
    async fn dreamer_prompt_carries_live_byte_caps() {
        use std::sync::Arc;
        use std::sync::Mutex;

        use async_trait::async_trait;
        use llm::{
            ChatCompletionRequest, LlmClient, LlmError, Role, ToolChatCompletionRequest,
            ToolChatCompletionResponse,
        };
        use tempfile::TempDir;

        use crate::settings::{Settings, SettingsHandle};

        struct SystemRecorder(Mutex<String>);
        #[async_trait]
        impl LlmClient for SystemRecorder {
            async fn chat_completion(&self, _req: ChatCompletionRequest) -> llm::Result<String> {
                Err(LlmError::Provider {
                    status: 0,
                    body: "not used".into(),
                })
            }
            async fn chat_completion_with_tools(
                &self,
                req: ToolChatCompletionRequest,
            ) -> llm::Result<ToolChatCompletionResponse> {
                if let Some(m) = req.messages.iter().find(|m| m.role == Role::System) {
                    self.0.lock().unwrap().clone_from(&m.content);
                }
                Ok(ToolChatCompletionResponse::Message("done".into()))
            }
        }

        let dir = TempDir::new().unwrap();
        let mut s = Settings::compiled_defaults();
        s.ai.profiles[0].model = "model-v1".into();
        s.ai.memory.soul_bytes = 9001;
        s.ai.memory.lore_bytes = 40_000;
        s.ai.memory.user_bytes = 12_345;
        s.ai.memory.scratch_bytes = 4321;
        s.ai.memory.inject_byte_budget = 131_072;
        let settings: SettingsHandle = SettingsHandle::from_settings(s);
        let store = crate::ai::memory::store::MemoryStore::open(dir.path(), settings.clone())
            .await
            .unwrap();
        let transcript =
            crate::ai::memory::transcript::TranscriptWriter::open(store.memories_dir())
                .await
                .unwrap();

        let rec = Arc::new(SystemRecorder(Mutex::new(String::new())));
        let registry = crate::llm_factory::LlmRegistry::fixed(
            rec.clone() as Arc<dyn LlmClient>,
            settings.clone(),
        );
        run_ritual(
            &registry,
            &store,
            &transcript,
            &settings,
            jiff::civil::date(2026, 4, 17),
            None,
        )
        .await
        .unwrap();

        let prompt = rec.0.lock().unwrap().clone();
        assert!(
            !prompt.contains("{caps}"),
            "placeholder left unsubstituted in dreamer prompt"
        );
        for n in ["9001", "40000", "12345", "4321"] {
            assert!(
                prompt.contains(n),
                "live cap {n} missing from dreamer prompt:\n{prompt}"
            );
        }
    }

    /// An empty model makes every request fail with `No models provided`, so
    /// the ritual skips the LLM entirely — but it must still rotate, or
    /// today.md grows forever with no date boundary.
    #[tokio::test]
    async fn run_ritual_with_empty_model_still_rotates() {
        use std::sync::Arc;
        use std::sync::Mutex;

        use async_trait::async_trait;
        use llm::{
            ChatCompletionRequest, LlmClient, LlmError, ToolChatCompletionRequest,
            ToolChatCompletionResponse,
        };
        use tempfile::TempDir;

        use crate::settings::{Settings, SettingsHandle};

        struct Exploding(Mutex<usize>);
        #[async_trait]
        impl LlmClient for Exploding {
            async fn chat_completion(&self, _req: ChatCompletionRequest) -> llm::Result<String> {
                unreachable!("empty model must not reach the provider");
            }
            async fn chat_completion_with_tools(
                &self,
                _req: ToolChatCompletionRequest,
            ) -> llm::Result<ToolChatCompletionResponse> {
                *self.0.lock().unwrap() += 1;
                Err(LlmError::Provider {
                    status: 400,
                    body: "No models provided".into(),
                })
            }
        }

        let dir = TempDir::new().unwrap();
        // compiled_defaults already carries an empty connection.model.
        let settings: SettingsHandle = SettingsHandle::from_settings(Settings::compiled_defaults());
        let store = crate::ai::memory::store::MemoryStore::open(dir.path(), settings.clone())
            .await
            .unwrap();
        let transcript =
            crate::ai::memory::transcript::TranscriptWriter::open(store.memories_dir())
                .await
                .unwrap();

        let client = Arc::new(Exploding(Mutex::new(0)));
        let registry = crate::llm_factory::LlmRegistry::fixed(
            client.clone() as Arc<dyn LlmClient>,
            settings.clone(),
        );
        let yesterday = jiff::civil::date(2026, 4, 17);
        run_ritual(&registry, &store, &transcript, &settings, yesterday, None)
            .await
            .unwrap();

        assert_eq!(
            *client.0.lock().unwrap(),
            0,
            "empty model must not issue an LLM call",
        );
        assert!(
            transcript.dated_path_for(yesterday).exists(),
            "transcript must still rotate when the ritual skips the LLM",
        );
    }
}
