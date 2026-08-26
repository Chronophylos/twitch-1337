//! Background Recap batcher: paraphrases eligible chat lines via a tiny model.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use llm::{ChatCompletionRequest, LlmClient, LlmError, Message, Result as LlmResult, TraceIds};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::ai::chat_history::ChatHistory;
use crate::ai::recap::{
    RECAP_BACKOFF_CAP, RECAP_BACKOFF_INITIAL, RECAP_FLUSH_AGE, RECAP_MAX_GENERATION,
    RECAP_PARK_FAILURES, RecapChannelLog, RecapFile, RecapHandle, RecapIdentity, RecapNote,
    RecapState, append_note, eligible, format_batch_line, identities_from_batch,
    rebind_through_seq,
};
use crate::settings::SettingsHandle;

const TICK_INTERVAL: Duration = Duration::from_secs(1);

async fn paraphrase(
    llm: &dyn LlmClient,
    model: &str,
    timeout: Duration,
    user: String,
) -> LlmResult<String> {
    let req = ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message::system(twitch_1337_data::prompts::RECAP),
            Message::user(user),
        ],
        reasoning_effort: None,
        service_tier: None,
        trace: TraceIds {
            user: Some("<recap>".into()),
            session_id: Some(crate::ai::session::new_session_id()),
        },
    };
    tokio::time::timeout(timeout, llm.chat_completion(req))
        .await
        .map_err(|_| LlmError::Timeout)?
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
    llm: &dyn LlmClient,
    model: &str,
    timeout: Duration,
) -> Result<(), LlmError> {
    let folded: Vec<RecapNote> = indices.iter().map(|&i| log.notes[i].clone()).collect();
    let user = folded
        .iter()
        .map(|n| n.body.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let body = paraphrase(llm, model, timeout, user).await?;
    let insert_at = *indices.first().expect("non-empty fold");
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

pub async fn coarsen_if_needed(
    log: &mut RecapChannelLog,
    max_notes: usize,
    merge_width: usize,
    llm: Option<&dyn LlmClient>,
    model: &str,
    timeout: Duration,
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
                let result = match llm {
                    Some(llm) => apply_coarsen_fold(log, &indices, llm, model, timeout).await,
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
    let oldest = eligible[0];
    if oldest.timestamp < Utc::now() - RECAP_FLUSH_AGE {
        return true;
    }
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
    model: &'a str,
    timeout: Duration,
    self_logins: &'a [String],
}

async fn try_flush_channel(params: FlushParams<'_>) -> FlushAttempt {
    let FlushParams {
        mut input,
        llm,
        model,
        timeout,
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
    let user = batch
        .iter()
        .map(|e| format_batch_line(e, self_logins))
        .collect::<Vec<_>>()
        .join("\n");

    let body = match paraphrase(llm, model, timeout, user).await {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(?error, "recap batch paraphrase failed");
            return FlushAttempt::LlmFailed;
        }
    };

    let last = batch.last().expect("non-empty batch");
    let identities = identities_from_batch(&batch);
    let through = last.seq;
    append_note(&mut input.log, body, identities, through, last);
    if let Err(error) = coarsen_if_needed(
        &mut input.log,
        input.max_notes,
        input.merge_width,
        Some(llm),
        model,
        timeout,
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
    ai_through_seq: u64,
}

pub(crate) struct BatcherTickParams<'a> {
    pub handle: &'a RecapHandle,
    pub settings: &'a SettingsHandle,
    pub llm: &'a dyn LlmClient,
    pub primary: &'a ChatHistory,
    pub ai: Option<&'a ChatHistory>,
    pub self_logins: &'a [String],
    pub last_seen_model: &'a mut Option<String>,
    pub backoff: &'a mut BatcherBackoff,
}

/// One batcher iteration: snapshot settings, optionally flush each channel.
pub(crate) async fn recap_batcher_tick(params: BatcherTickParams<'_>) {
    let BatcherTickParams {
        handle,
        settings,
        llm,
        primary,
        ai,
        self_logins,
        last_seen_model,
        backoff,
    } = params;
    let snap = settings.load();
    let model = match snap.ai.history.recap_model.as_deref() {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => {
            *last_seen_model = None;
            backoff.on_success();
            return;
        }
    };

    if last_seen_model.as_deref() != Some(model.as_str()) {
        *last_seen_model = Some(model.clone());
        let mut guard = handle.inner.lock().await;
        guard.parked = false;
        guard.failures = 0;
        backoff.on_success();
    }

    let timeout = Duration::from_secs(snap.ai.connection.timeout);
    let n = snap.ai.history.n;
    let ai_n = snap.ai.history.ai_channel_n;
    let primary_capacity = snap.ai.history.length as usize;
    let ai_capacity = snap.ai.history.ai_channel_length as usize;
    let batch_size = snap.ai.history.batch_size as usize;
    let max_notes = snap.ai.history.max_notes as usize;
    let merge_width = snap.ai.history.merge_width as usize;
    drop(snap);

    let TickSnapshot {
        path,
        mut file,
        mut primary_through_seq,
        mut ai_through_seq,
    } = {
        let guard = handle.inner.lock().await;
        if guard.parked {
            return;
        }
        TickSnapshot {
            path: guard.path.clone(),
            file: guard.file.clone(),
            primary_through_seq: guard.primary_through_seq,
            ai_through_seq: guard.ai_through_seq,
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
        model: &model,
        timeout,
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

    if let Some(ai_hist) = ai {
        let ai_entries = ai_hist.lock().await.snapshot();
        match try_flush_channel(FlushParams {
            input: FlushInput {
                log: file.ai_channel.clone(),
                through_seq: ai_through_seq,
                entries: ai_entries,
                capacity: ai_capacity,
                n: ai_n,
                batch_size,
                max_notes,
                merge_width,
            },
            llm,
            model: &model,
            timeout,
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
                file.ai_channel = out.log;
                ai_through_seq = out.through_seq;
            }
        }
    }

    let dirty = {
        let guard = handle.inner.lock().await;
        guard.file != file
            || guard.primary_through_seq != primary_through_seq
            || guard.ai_through_seq != ai_through_seq
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
    guard.ai_through_seq = ai_through_seq;
    guard.failures = 0;
    backoff.on_success();
}

/// Background loop: never blocks chat turns.
pub async fn run_recap_batcher(
    handle: RecapHandle,
    settings: SettingsHandle,
    llm: Arc<dyn LlmClient>,
    primary: ChatHistory,
    ai: Option<ChatHistory>,
    self_logins: Vec<String>,
    shutdown: Arc<Notify>,
) {
    let mut last_seen_model: Option<String> = None;
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
                recap_batcher_tick(BatcherTickParams {
                    handle: &handle,
                    settings: &settings,
                    llm: llm.as_ref(),
                    primary: &primary,
                    ai: ai.as_ref(),
                    self_logins: &self_logins,
                    last_seen_model: &mut last_seen_model,
                    backoff: &mut backoff,
                })
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use chrono::Utc;
    use tokio::sync::Mutex;

    use super::*;
    use crate::ai::chat_history::{ChatHistoryBuffer, ChatHistorySource};
    use crate::ai::recap::trim_note_body;
    use crate::settings::Settings;
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

    fn test_settings(recap_model: &str) -> SettingsHandle {
        let mut s = Settings::compiled_defaults();
        s.ai.history.recap_model = Some(recap_model.into());
        s.ai.history.n = 2;
        s.ai.history.batch_size = 1;
        Arc::new(ArcSwap::from_pointee(s))
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
            timestamp: Utc::now() - RECAP_FLUSH_AGE - chrono::Duration::seconds(1),
        }
    }

    #[tokio::test]
    async fn five_failures_park_and_stop() {
        let calls = Arc::new(AtomicU32::new(0));
        let llm = CountingBoom {
            calls: Arc::clone(&calls),
        };
        let settings = test_settings("tiny-model");
        let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(
            Arc::clone(&settings),
            crate::ai::chat_history::primary_history_capacity,
        )));
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
                ai: None,
                self_logins: &[],
                last_seen_model: &mut last_seen,
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
            ai: None,
            self_logins: &[],
            last_seen_model: &mut last_seen,
            backoff: &mut backoff,
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn trim_note_body_still_used_by_batcher_path() {
        let long = "x".repeat(900);
        assert!(trim_note_body(&long).len() <= crate::ai::recap::RECAP_NOTE_BYTES);
    }

    #[test]
    fn plan_coarsen_skips_high_gen_prefix() {
        let notes = vec![
            RecapNote {
                body: "high".into(),
                identities: vec![],
                through_seq: 1,
                generation: RECAP_MAX_GENERATION,
            },
            RecapNote {
                body: "a".into(),
                identities: vec![],
                through_seq: 2,
                generation: 0,
            },
            RecapNote {
                body: "b".into(),
                identities: vec![],
                through_seq: 3,
                generation: 0,
            },
        ];
        let action = plan_coarsen_step(&notes, 2);
        assert!(matches!(action, Some(CoarsenAction::Fold(indices)) if indices == vec![1, 2]));
    }

    #[tokio::test]
    async fn coarsen_fold_skips_high_gen_prefix() {
        let mut log = RecapChannelLog {
            notes: vec![
                RecapNote {
                    body: "high".into(),
                    identities: vec![],
                    through_seq: 1,
                    generation: RECAP_MAX_GENERATION,
                },
                RecapNote {
                    body: "a".into(),
                    identities: vec![],
                    through_seq: 2,
                    generation: 0,
                },
                RecapNote {
                    body: "b".into(),
                    identities: vec![],
                    through_seq: 3,
                    generation: 0,
                },
                RecapNote {
                    body: "c".into(),
                    identities: vec![],
                    through_seq: 4,
                    generation: 0,
                },
            ],
            ..Default::default()
        };
        coarsen_if_needed(&mut log, 3, 2, Some(&OkLlm), "m", Duration::from_secs(1))
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
            notes: vec![
                RecapNote {
                    body: "a".into(),
                    identities: vec![],
                    through_seq: 1,
                    generation: 0,
                },
                RecapNote {
                    body: "b".into(),
                    identities: vec![],
                    through_seq: 2,
                    generation: 0,
                },
                RecapNote {
                    body: "c".into(),
                    identities: vec![],
                    through_seq: 3,
                    generation: 0,
                },
            ],
            ..Default::default()
        };
        let boom = CountingBoom {
            calls: Arc::new(AtomicU32::new(0)),
        };
        coarsen_if_needed(&mut log, 1, 2, Some(&boom), "m", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(log.notes.len(), 1);
        assert_eq!(log.notes[0].body, "c");
    }

    #[test]
    fn non_full_buffer_does_not_flush_for_eviction_backpressure() {
        let mut entries = vec![grok_entry(1)];
        entries[0].timestamp = Utc::now();
        assert!(!should_flush(&[&entries[0]], 40, &entries, 200));
    }

    #[test]
    fn full_buffer_flushes_oldest_eligible_before_eviction() {
        let mut entries: Vec<_> = (1..=42).map(grok_entry).collect();
        for entry in &mut entries {
            entry.timestamp = Utc::now();
        }
        assert!(should_flush(&[&entries[0]], 40, &entries, 42));
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
    async fn render_suffix_while_batcher_on_llm_does_not_block() {
        use crate::ai::recap::{RecapChannel, RecapHandle};

        let dir = tempfile::tempdir().unwrap();
        let handle = RecapHandle::load(dir.path().to_path_buf()).await;
        {
            let mut guard = handle.inner.lock().await;
            guard.file.primary.notes.push(RecapNote {
                body: "live".into(),
                identities: vec![],
                through_seq: 0,
                generation: 0,
            });
        }

        let settings = test_settings("tiny-model");
        let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(
            Arc::clone(&settings),
            crate::ai::chat_history::primary_history_capacity,
        )));
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
                ai: None,
                self_logins: &[],
                last_seen_model: &mut last_seen,
                backoff: &mut backoff,
            })
            .await;
        });

        let started = Instant::now();
        let suffix = handle
            .render_suffix(RecapChannel::Primary, "main", "ai", 8)
            .await;
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(suffix.contains("live"));
        let _ = tick.await;
    }

    #[tokio::test]
    async fn persist_failure_does_not_advance_watermark() {
        use crate::ai::recap::RecapHandle;

        let settings = test_settings("tiny-model");
        let primary = Arc::new(Mutex::new(ChatHistoryBuffer::new(
            Arc::clone(&settings),
            crate::ai::chat_history::primary_history_capacity,
        )));
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
            ai: None,
            self_logins: &[],
            last_seen_model: &mut last_seen,
            backoff: &mut backoff,
        })
        .await;

        let guard = handle.inner.lock().await;
        assert_eq!(guard.primary_through_seq, 0);
        assert!(guard.file.primary.notes.is_empty());
        assert_eq!(guard.failures, 1);
    }
}
