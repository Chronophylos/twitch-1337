//! Background Recap batcher: paraphrases eligible chat lines via a tiny model.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use llm::{ChatCompletionRequest, LlmClient, LlmError, Message, Result as LlmResult, TraceIds};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::ai::chat_history::{ChatHistory, eligible};
use crate::ai::recap::{
    RECAP_BACKOFF_CAP, RECAP_BACKOFF_INITIAL, RECAP_FLUSH_AGE, RECAP_MAX_GENERATION,
    RECAP_PARK_FAILURES, RecapChannelLog, RecapFile, RecapHandle, RecapIdentity, RecapNote,
    RecapState, append_note, format_batch_line, identities_from_batch, rebind_through_seq,
};
use crate::settings::SettingsHandle;
use crate::util::observability;

const TICK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(crate) struct RecapCompletion<'a> {
    llm: &'a dyn LlmClient,
    /// Recap profile name, for the Turn span.
    profile: &'a str,
    model: &'a str,
    timeout: Duration,
    reasoning_effort: Option<&'a str>,
    service_tier: Option<&'a str>,
}

#[expect(
    clippy::map_err_ignore,
    reason = "tokio Elapsed carries no information beyond the timeout"
)]
async fn paraphrase(call: RecapCompletion<'_>, user: String) -> LlmResult<String> {
    let session_id = crate::ai::session::new_session_id();
    let req = ChatCompletionRequest {
        model: call.model.to_string(),
        messages: vec![
            Message::system(twitch_1337_data::prompts::RECAP),
            Message::user(user),
        ],
        reasoning_effort: call.reasoning_effort.map(str::to_string),
        service_tier: call.service_tier.map(str::to_string),
        trace: TraceIds {
            user: Some("<recap>".into()),
            session_id: Some(session_id.clone()),
        },
    };
    observability::with_turn("recap", call.profile, &session_id, None, async {
        tokio::time::timeout(call.timeout, call.llm.chat_completion(req))
            .await
            .map_err(|_| LlmError::Timeout)?
    })
    .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoarsenAction {
    Drop(usize),
    Fold(Vec<usize>),
}

/// Oldest `merge_width` notes at the minimum generation (not necessarily a vec prefix).
pub(crate) fn plan_coarsen_step(notes: &[RecapNote], merge_width: usize) -> Option<CoarsenAction> {
    if notes.is_empty() {
        return None;
    }
    let min_gen = notes.iter().map(|n| n.generation).min()?;
    let indices: Vec<usize> = notes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.generation == min_gen)
        .take(merge_width)
        .map(|(i, _)| i)
        .collect();
    if indices.is_empty() {
        return None;
    }
    if min_gen >= RECAP_MAX_GENERATION {
        return Some(CoarsenAction::Drop(indices[0]));
    }
    if indices.len() < 2 {
        return Some(CoarsenAction::Drop(indices[0]));
    }
    Some(CoarsenAction::Fold(indices))
}

fn union_identities(notes: &[RecapNote]) -> Vec<RecapIdentity> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for note in notes {
        for id in &note.identities {
            let key = id.login.to_ascii_lowercase();
            if seen.insert(key) {
                out.push(id.clone());
            }
        }
    }
    out
}

async fn apply_coarsen_fold(
    log: &mut RecapChannelLog,
    indices: &[usize],
    call: RecapCompletion<'_>,
) -> Result<(), LlmError> {
    let Some(&insert_at) = indices.first() else {
        return Ok(());
    };
    let folded: Vec<RecapNote> = indices.iter().map(|&i| log.notes[i].clone()).collect();
    let user = folded
        .iter()
        .map(|n| n.body.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let body = paraphrase(call, user).await?;
    let max_through = folded.iter().map(|n| n.through_seq).max().unwrap_or(0);
    let max_gen = folded.iter().map(|n| n.generation).max().unwrap_or(0);
    let merged = RecapNote {
        body: crate::ai::recap::trim_note_body(&body),
        identities: union_identities(&folded),
        through_seq: max_through,
        generation: max_gen.saturating_add(1),
    };
    for &i in indices.iter().rev() {
        log.notes.remove(i);
    }
    log.notes.insert(insert_at, merged);
    Ok(())
}

pub(crate) async fn coarsen_if_needed(
    log: &mut RecapChannelLog,
    max_notes: usize,
    merge_width: usize,
    call: Option<RecapCompletion<'_>>,
) -> Result<(), LlmError> {
    while log.notes.len() > max_notes {
        let Some(action) = plan_coarsen_step(&log.notes, merge_width) else {
            break;
        };
        match action {
            CoarsenAction::Drop(idx) => {
                log.notes.remove(idx);
                tracing::warn!(
                    generation = RECAP_MAX_GENERATION,
                    "dropped oldest recap note at max generation"
                );
            }
            CoarsenAction::Fold(indices) => {
                let result = match call {
                    Some(call) => apply_coarsen_fold(log, &indices, call).await,
                    None => Err(LlmError::EmptyResponse),
                };
                if let Err(error) = result {
                    log.notes.remove(0);
                    tracing::warn!(
                        ?error,
                        "recap coarsening failed; dropped oldest recap note instead"
                    );
                }
            }
        }
    }

    let ceiling = max_notes.saturating_mul(2);
    while log.notes.len() > ceiling {
        log.notes.remove(0);
        tracing::warn!(
            len = log.notes.len(),
            ceiling,
            "drained oldest recap note past hard ceiling"
        );
    }

    Ok(())
}

fn should_flush(
    eligible: &[&crate::ai::chat_history::ChatHistoryEntry],
    batch_size: usize,
    snapshot: &[crate::ai::chat_history::ChatHistoryEntry],
    capacity: usize,
) -> bool {
    if eligible.is_empty() {
        return false;
    }
    if eligible.len() >= batch_size {
        return true;
    }
    // Eligible lines are already older than the N-window, so their own
    // timestamps are usually past FLUSH_AGE the moment they qualify. Age the
    // newest buffer line instead: a live tail accumulates toward batch_size;
    // a quiet channel still flushes a leftover partial batch.
    if snapshot
        .last()
        .is_some_and(|e| e.timestamp < Timestamp::now() - RECAP_FLUSH_AGE)
    {
        return true;
    }
    let oldest = eligible[0];
    snapshot
        .iter()
        .position(|e| e.seq == oldest.seq)
        .is_some_and(|idx| capacity.saturating_sub(snapshot.len()).saturating_add(idx) < batch_size)
}

struct FlushInput {
    log: RecapChannelLog,
    through_seq: u64,
    entries: Vec<crate::ai::chat_history::ChatHistoryEntry>,
    capacity: usize,
    n: u64,
    batch_size: usize,
    max_notes: usize,
    merge_width: usize,
}

struct FlushOutput {
    log: RecapChannelLog,
    through_seq: u64,
}

enum FlushAttempt {
    Idle,
    Updated(FlushOutput),
    LlmFailed,
}

struct FlushParams<'a> {
    input: FlushInput,
    llm: &'a dyn LlmClient,
    profile: &'a str,
    model: &'a str,
    timeout: Duration,
    reasoning_effort: Option<&'a str>,
    service_tier: Option<&'a str>,
    self_logins: &'a [String],
}

async fn try_flush_channel(params: FlushParams<'_>) -> FlushAttempt {
    let FlushParams {
        mut input,
        llm,
        profile,
        model,
        timeout,
        reasoning_effort,
        service_tier,
        self_logins,
    } = params;
    let entries = &input.entries;
    if input.through_seq == 0 && input.log.through_fp.is_some() {
        input.through_seq = rebind_through_seq(&input.log, entries);
    }

    let eligible = eligible(entries, input.n as usize, input.through_seq, self_logins);
    if !should_flush(&eligible, input.batch_size, entries, input.capacity) {
        return FlushAttempt::Idle;
    }

    let take = eligible.len().min(input.batch_size);
    let batch: Vec<_> = eligible.into_iter().take(take).collect();
    let Some(last) = batch.last() else {
        return FlushAttempt::Idle;
    };
    let user = batch
        .iter()
        .map(|e| format_batch_line(e, self_logins))
        .collect::<Vec<_>>()
        .join("\n");

    let body = match paraphrase(
        RecapCompletion {
            llm,
            profile,
            model,
            timeout,
            reasoning_effort,
            service_tier,
        },
        user,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(?error, "recap batch paraphrase failed");
            return FlushAttempt::LlmFailed;
        }
    };

    let identities = identities_from_batch(&batch);
    let through = last.seq;
    append_note(&mut input.log, body, identities, through, last);
    if let Err(error) = coarsen_if_needed(
        &mut input.log,
        input.max_notes,
        input.merge_width,
        Some(RecapCompletion {
            llm,
            profile,
            model,
            timeout,
            reasoning_effort,
            service_tier,
        }),
    )
    .await
    {
        tracing::warn!(?error, "recap coarsening failed");
        return FlushAttempt::LlmFailed;
    }

    FlushAttempt::Updated(FlushOutput {
        log: input.log,
        through_seq: through,
    })
}

pub(crate) struct BatcherBackoff {
    pub current: Duration,
    pub until: Option<Instant>,
}

impl BatcherBackoff {
    pub fn on_failure(&mut self) {
        let next = if self.current.is_zero() {
            RECAP_BACKOFF_INITIAL
        } else {
            (self.current * 2).min(RECAP_BACKOFF_CAP)
        };
        self.current = next;
        self.until = Some(Instant::now() + next);
    }

    pub fn on_success(&mut self) {
        self.current = Duration::ZERO;
        self.until = None;
    }
}

fn record_failure(state: &mut RecapState, backoff: &mut BatcherBackoff) {
    state.failures = state.failures.saturating_add(1);
    backoff.on_failure();
    if state.failures >= RECAP_PARK_FAILURES {
        state.parked = true;
        tracing::warn!(
            failures = state.failures,
            "recap batcher parked after consecutive failures"
        );
    }
}

struct TickSnapshot {
    path: std::path::PathBuf,
    file: RecapFile,
    primary_through_seq: u64,
}

pub(crate) struct BatcherTickParams<'a> {
    pub handle: &'a RecapHandle,
    pub settings: &'a SettingsHandle,
    pub llm: &'a dyn LlmClient,
    pub primary: &'a ChatHistory,
    pub self_logins: &'a [String],
    pub last_seen_profile: &'a mut Option<String>,
    pub backoff: &'a mut BatcherBackoff,
}

/// One batcher iteration: snapshot settings, optionally flush the chat log.
pub(crate) async fn recap_batcher_tick(params: BatcherTickParams<'_>) {
    let BatcherTickParams {
        handle,
        settings,
        llm,
        primary,
        self_logins,
        last_seen_profile,
        backoff,
    } = params;
    let snap = settings.load();
    let profile_name = match snap.ai.recap.profile.as_deref() {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => {
            *last_seen_profile = None;
            backoff.on_success();
            return;
        }
    };

    if last_seen_profile.as_deref() != Some(profile_name.as_str()) {
        *last_seen_profile = Some(profile_name.clone());
        let mut guard = handle.inner.lock().await;
        guard.parked = false;
        guard.failures = 0;
        backoff.on_success();
    }

    let Some(prof) = snap.ai.by_name(&profile_name).cloned() else {
        tracing::warn!(
            profile = %profile_name,
            "recap profile missing, skipping tick"
        );
        return;
    };
    if prof.model.trim().is_empty() {
        tracing::warn!(
            profile = %profile_name,
            "recap profile has an empty model, skipping tick"
        );
        return;
    }
    let timeout = Duration::from_secs(prof.timeout);
    let model = prof.model.clone();
    let reasoning_effort = prof.reasoning_effort.clone();
    let service_tier = prof.service_tier.clone();

    let n = snap.ai.history.n;
    let primary_capacity = snap.ai.history.length as usize;
    let batch_size = snap.ai.recap.batch_size as usize;
    let max_notes = snap.ai.recap.max_notes as usize;
    let merge_width = snap.ai.recap.merge_width as usize;
    drop(snap);

    let TickSnapshot {
        path,
        mut file,
        mut primary_through_seq,
    } = {
        let guard = handle.inner.lock().await;
        if guard.parked {
            return;
        }
        TickSnapshot {
            path: guard.path.clone(),
            file: guard.file.clone(),
            primary_through_seq: guard.primary_through_seq,
        }
    };

    let primary_entries = primary.lock().await.snapshot();
    match try_flush_channel(FlushParams {
        input: FlushInput {
            log: file.primary.clone(),
            through_seq: primary_through_seq,
            entries: primary_entries,
            capacity: primary_capacity,
            n,
            batch_size,
            max_notes,
            merge_width,
        },
        llm,
        profile: &profile_name,
        model: &model,
        timeout,
        reasoning_effort: reasoning_effort.as_deref(),
        service_tier: service_tier.as_deref(),
        self_logins,
    })
    .await
    {
        FlushAttempt::Idle => {}
        FlushAttempt::LlmFailed => {
            let mut guard = handle.inner.lock().await;
            record_failure(&mut guard, backoff);
            return;
        }
        FlushAttempt::Updated(out) => {
            file.primary = out.log;
            primary_through_seq = out.through_seq;
        }
    }

    let dirty = {
        let guard = handle.inner.lock().await;
        guard.file != file || guard.primary_through_seq != primary_through_seq
    };
    if !dirty {
        return;
    }

    if let Err(error) = crate::util::persist::atomic_save_ron_async(&file, &path).await {
        tracing::error!(?error, path = %path.display(), "chat-recap.ron save failed");
        let mut guard = handle.inner.lock().await;
        record_failure(&mut guard, backoff);
        return;
    }

    let mut guard = handle.inner.lock().await;
    guard.file = file;
    guard.primary_through_seq = primary_through_seq;
    guard.failures = 0;
    backoff.on_success();
}

/// Background loop: never blocks chat turns.
pub async fn run_recap_batcher(
    handle: RecapHandle,
    settings: SettingsHandle,
    registry: Arc<crate::llm_factory::LlmRegistry>,
    primary: ChatHistory,
    self_logins: Vec<String>,
    shutdown: Arc<Notify>,
) {
    let mut last_seen_profile: Option<String> = None;
    let mut backoff = BatcherBackoff {
        current: Duration::ZERO,
        until: None,
    };
    loop {
        let sleep_for = match backoff.until {
            Some(until) if Instant::now() < until => until - Instant::now(),
            _ => TICK_INTERVAL,
        };
        tokio::select! {
            () = shutdown.notified() => break,
            () = tokio::time::sleep(sleep_for) => {
                let snap = settings.load();
                let Some(name) = snap
                    .ai
                    .recap
                    .profile
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                else {
                    last_seen_profile = None;
                    backoff.on_success();
                    continue;
                };
                let Some((client, _)) = registry.resolve_named(name) else {
                    tracing::debug!(
                        profile = name,
                        "recap skipped, named profile unusable"
                    );
                    continue;
                };
                let tick = recap_batcher_tick(BatcherTickParams {
                    handle: &handle,
                    settings: &settings,
                    llm: client.as_ref(),
                    primary: &primary,
                    self_logins: &self_logins,
                    last_seen_profile: &mut last_seen_profile,
                    backoff: &mut backoff,
                });
                crate::util::observability::metrics::with_llm_scope("recap", name, tick).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;
    use jiff::SignedDuration;
    use tokio::sync::Mutex;

    use super::*;
    use crate::ai::chat_history::{ChatHistoryBuffer, ChatHistorySource};
    use crate::settings::Settings;
    use crate::settings::ai::AiProfile;
    use crate::settings::provider::AiProvider;
    use crate::twitch::mouth::Agent;

    struct CountingBoom {
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl LlmClient for CountingBoom {
        async fn chat_completion(&self, _: ChatCompletionRequest) -> LlmResult<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(LlmError::EmptyResponse)
        }

        async fn chat_completion_with_tools(
            &self,
            _: llm::ToolChatCompletionRequest,
        ) -> LlmResult<llm::ToolChatCompletionResponse> {
            unimplemented!()
        }
    }

    struct OkLlm;

    #[async_trait]
    impl LlmClient for OkLlm {
        async fn chat_completion(&self, _: ChatCompletionRequest) -> LlmResult<String> {
            Ok("zusammengefasst".into())
        }

        async fn chat_completion_with_tools(
            &self,
            _: llm::ToolChatCompletionRequest,
        ) -> LlmResult<llm::ToolChatCompletionResponse> {
            unimplemented!()
        }
    }

    fn test_settings(recap_profile: &str) -> SettingsHandle {
        let mut s = Settings::compiled_defaults();
        if recap_profile.is_empty() {
            s.ai.recap.profile = None;
        } else {
            s.ai.recap.profile = Some(recap_profile.into());
            s.ai.profiles.push(AiProfile {
                name: recap_profile.into(),
                key: None,
                provider: AiProvider::Ollama,
                base_url: None,
                model: "tiny-model".into(),
                timeout: 30,
                reasoning_effort: None,
                service_tier: None,
                ..Default::default()
            });
        }
        s.ai.history.n = 2;
        s.ai.recap.batch_size = 1;
        SettingsHandle::from_settings(s)
    }

    fn grok_entry(seq: u64) -> crate::ai::chat_history::ChatHistoryEntry {
        crate::ai::chat_history::ChatHistoryEntry {
            seq,
            username: "grok".into(),
            display_name: Some("Grok".into()),
            user_id: None,
            text: "hello".into(),
            source: ChatHistorySource::Bot,
            agent: Some(Agent::Grok),
            timestamp: Timestamp::now() - RECAP_FLUSH_AGE - SignedDuration::from_secs(1),
        }
    }

    /// Primary buffer with three viewers and one flush-eligible Grok line.
    async fn seeded_primary(settings: &SettingsHandle) -> Arc<Mutex<ChatHistoryBuffer>> {
        let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(settings.clone())));
        {
            let mut buf = primary.lock().await;
            buf.push_user("v1", "a");
            buf.push_user("v2", "b");
            buf.push_user("v3", "c");
            let g = grok_entry(4);
            buf.push_bot_with_identity_at(
                g.username,
                g.display_name.as_deref(),
                Agent::Grok,
                g.text,
                g.timestamp,
            );
        }
        primary
    }

    fn note(body: &str, through_seq: u64, generation: u32) -> RecapNote {
        RecapNote {
            body: body.into(),
            identities: vec![],
            through_seq,
            generation,
        }
    }

    fn completion(llm: &dyn LlmClient) -> RecapCompletion<'_> {
        RecapCompletion {
            llm,
            profile: "recap",
            model: "m",
            timeout: Duration::from_secs(1),
            reasoning_effort: None,
            service_tier: None,
        }
    }

    #[tokio::test]
    async fn five_failures_park_and_stop() {
        let calls = Arc::new(AtomicU32::new(0));
        let llm = CountingBoom {
            calls: Arc::clone(&calls),
        };
        let settings = test_settings("tiny-model");
        let primary = seeded_primary(&settings).await;

        let dir = tempfile::tempdir().unwrap();
        let handle = RecapHandle::load(dir.path().to_path_buf()).await;
        let mut last_seen = None;
        let mut backoff = BatcherBackoff {
            current: Duration::ZERO,
            until: None,
        };

        for _ in 0..5 {
            recap_batcher_tick(BatcherTickParams {
                handle: &handle,
                settings: &settings,
                llm: &llm,
                primary: &primary,
                self_logins: &[],
                last_seen_profile: &mut last_seen,
                backoff: &mut backoff,
            })
            .await;
        }

        assert!(handle.parked().await);
        assert_eq!(calls.load(Ordering::SeqCst), 5);

        recap_batcher_tick(BatcherTickParams {
            handle: &handle,
            settings: &settings,
            llm: &llm,
            primary: &primary,
            self_logins: &[],
            last_seen_profile: &mut last_seen,
            backoff: &mut backoff,
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn plan_coarsen_skips_high_gen_prefix() {
        let notes = vec![
            note("high", 1, RECAP_MAX_GENERATION),
            note("a", 2, 0),
            note("b", 3, 0),
        ];
        let action = plan_coarsen_step(&notes, 2);
        assert!(matches!(action, Some(CoarsenAction::Fold(indices)) if indices == vec![1, 2]));
    }

    #[tokio::test]
    async fn coarsen_fold_skips_high_gen_prefix() {
        let mut log = RecapChannelLog {
            notes: vec![
                note("high", 1, RECAP_MAX_GENERATION),
                note("a", 2, 0),
                note("b", 3, 0),
                note("c", 4, 0),
            ],
            ..Default::default()
        };
        coarsen_if_needed(&mut log, 3, 2, Some(completion(&OkLlm)))
            .await
            .unwrap();
        assert_eq!(log.notes.len(), 3);
        assert_eq!(log.notes[0].body, "high");
        assert_eq!(log.notes[1].body, "zusammengefasst");
        assert_eq!(log.notes[1].generation, 1);
        assert_eq!(log.notes[2].body, "c");
    }

    #[tokio::test]
    async fn coarsen_fold_failure_drops_oldest_instead_of_parking() {
        let mut log = RecapChannelLog {
            notes: vec![note("a", 1, 0), note("b", 2, 0), note("c", 3, 0)],
            ..Default::default()
        };
        let boom = CountingBoom {
            calls: Arc::new(AtomicU32::new(0)),
        };
        coarsen_if_needed(&mut log, 1, 2, Some(completion(&boom)))
            .await
            .unwrap();
        assert_eq!(log.notes.len(), 1);
        assert_eq!(log.notes[0].body, "c");
    }

    #[test]
    fn non_full_buffer_does_not_flush_for_eviction_backpressure() {
        let mut entries = vec![grok_entry(1)];
        entries[0].timestamp = Timestamp::now();
        assert!(!should_flush(&[&entries[0]], 40, &entries, 200));
    }

    #[test]
    fn full_buffer_flushes_oldest_eligible_before_eviction() {
        let mut entries: Vec<_> = (1..=42).map(grok_entry).collect();
        for entry in &mut entries {
            entry.timestamp = Timestamp::now();
        }
        assert!(should_flush(&[&entries[0]], 40, &entries, 42));
    }

    #[test]
    fn stale_eligible_does_not_flush_while_channel_is_live() {
        let mut entries = vec![grok_entry(1), grok_entry(2)];
        entries[0].timestamp = Timestamp::now() - RECAP_FLUSH_AGE - SignedDuration::from_secs(1);
        entries[1].timestamp = Timestamp::now();
        assert!(
            !should_flush(&[&entries[0]], 40, &entries, 200),
            "a live tail must accumulate toward batch_size; eligible lines are already N-window old"
        );
    }

    #[test]
    fn quiet_channel_flushes_partial_batch() {
        let mut entries = vec![grok_entry(1)];
        entries[0].timestamp = Timestamp::now() - RECAP_FLUSH_AGE - SignedDuration::from_secs(1);
        assert!(should_flush(&[&entries[0]], 40, &entries, 200));
    }

    struct SlowOkLlm;

    #[async_trait]
    impl LlmClient for SlowOkLlm {
        async fn chat_completion(&self, _: ChatCompletionRequest) -> LlmResult<String> {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok("ok".into())
        }

        async fn chat_completion_with_tools(
            &self,
            _: llm::ToolChatCompletionRequest,
        ) -> LlmResult<llm::ToolChatCompletionResponse> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn render_fenced_while_batcher_on_llm_does_not_block() {
        use crate::ai::recap::RecapHandle;

        let dir = tempfile::tempdir().unwrap();
        let handle = RecapHandle::load(dir.path().to_path_buf()).await;
        {
            let mut guard = handle.inner.lock().await;
            guard.file.primary.notes.push(note("live", 0, 0));
        }

        let settings = test_settings("tiny-model");
        let primary = seeded_primary(&settings).await;

        let handle_bg = RecapHandle {
            inner: Arc::clone(&handle.inner),
        };
        let llm = SlowOkLlm;
        let tick = tokio::spawn(async move {
            let mut last_seen = None;
            let mut backoff = BatcherBackoff {
                current: Duration::ZERO,
                until: None,
            };
            recap_batcher_tick(BatcherTickParams {
                handle: &handle_bg,
                settings: &settings,
                llm: &llm,
                primary: &primary,
                self_logins: &[],
                last_seen_profile: &mut last_seen,
                backoff: &mut backoff,
            })
            .await;
        });

        let started = Instant::now();
        let suffix = handle.render_fenced("main", 8, "n0nce").await;
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(suffix.contains("live"));
        tick.await.unwrap();
    }

    #[tokio::test]
    async fn persist_failure_does_not_advance_watermark() {
        use crate::ai::recap::RecapHandle;

        let settings = test_settings("tiny-model");
        let primary = seeded_primary(&settings).await;

        let dir = tempfile::tempdir().unwrap();
        let handle = RecapHandle::load(dir.path().to_path_buf()).await;
        {
            let mut guard = handle.inner.lock().await;
            guard.path = std::path::PathBuf::from("/definitely/not/writable/chat-recap.ron");
        }

        let mut last_seen = None;
        let mut backoff = BatcherBackoff {
            current: Duration::ZERO,
            until: None,
        };
        recap_batcher_tick(BatcherTickParams {
            handle: &handle,
            settings: &settings,
            llm: &OkLlm,
            primary: &primary,
            self_logins: &[],
            last_seen_profile: &mut last_seen,
            backoff: &mut backoff,
        })
        .await;

        let guard = handle.inner.lock().await;
        assert_eq!(guard.primary_through_seq, 0);
        assert!(guard.file.primary.notes.is_empty());
        assert_eq!(guard.failures, 1);
    }
}
