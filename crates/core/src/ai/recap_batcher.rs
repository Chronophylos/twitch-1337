//! Background Recap batcher: paraphrases eligible chat lines via a tiny model.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use llm::{ChatCompletionRequest, LlmClient, LlmError, Message, Result as LlmResult, TraceIds};
use tokio::sync::Notify;

use crate::ai::chat_history::ChatHistory;
use crate::ai::recap::{
    RECAP_FLUSH_AGE, RECAP_MAX_GENERATION, RECAP_PARK_FAILURES, RecapChannelLog, RecapHandle,
    RecapNote, RecapState, append_note, eligible, format_batch_line, identities_from_batch,
    rebind_through_seq,
};
use crate::settings::SettingsHandle;

/// Stub until Task 11 wires `twitch_1337_data::prompts::RECAP`.
pub(crate) const RECAP_PROMPT: &str =
    "Paraphrase the chat batch in German, third person, no quotes.";

const TICK_INTERVAL: Duration = Duration::from_secs(1);

async fn paraphrase(
    llm: &dyn LlmClient,
    model: &str,
    timeout: Duration,
    user: String,
) -> LlmResult<String> {
    let req = ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![Message::system(RECAP_PROMPT), Message::user(user)],
        reasoning_effort: None,
        service_tier: None,
        trace: TraceIds {
            user: Some("<recap>".into()),
            session_id: Some(crate::ai::session::new_session_id()),
        },
    };
    tokio::time::timeout(timeout, llm.chat_completion(req))
        .await
        .map_err(|_| LlmError::EmptyResponse)?
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
        let min_gen = log.notes.iter().map(|n| n.generation).min().unwrap_or(0);
        if min_gen >= RECAP_MAX_GENERATION {
            log.notes.remove(0);
            tracing::warn!(
                generation = min_gen,
                "dropped oldest recap note at max generation"
            );
            continue;
        }

        let fold_count = log
            .notes
            .iter()
            .take_while(|n| n.generation == min_gen)
            .count()
            .min(merge_width);
        if fold_count < 2 {
            log.notes.remove(0);
            tracing::warn!("dropped oldest recap note; not enough notes to fold");
            continue;
        }

        let folded: Vec<RecapNote> = log.notes.drain(..fold_count).collect();
        let max_through = folded.iter().map(|n| n.through_seq).max().unwrap_or(0);
        let max_gen = folded.iter().map(|n| n.generation).max().unwrap_or(0);
        let mut identities = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for note in &folded {
            for id in &note.identities {
                let key = id.login.to_ascii_lowercase();
                if seen.insert(key) {
                    identities.push(id.clone());
                }
            }
        }
        let user = folded
            .iter()
            .map(|n| n.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let llm = llm.ok_or(LlmError::EmptyResponse)?;
        let body = paraphrase(llm, model, timeout, user).await?;
        log.notes.insert(
            0,
            RecapNote {
                body: crate::ai::recap::trim_note_body(&body),
                identities,
                through_seq: max_through,
                generation: max_gen.saturating_add(1),
            },
        );
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
        .is_some_and(|idx| idx < batch_size)
}

enum ChannelFlush {
    Idle,
    Failed,
    Saved,
}

struct FlushParams<'a> {
    log: &'a mut RecapChannelLog,
    through_seq: &'a mut u64,
    entries: &'a [crate::ai::chat_history::ChatHistoryEntry],
    n: u64,
    batch_size: usize,
    max_notes: usize,
    merge_width: usize,
    llm: &'a dyn LlmClient,
    model: &'a str,
    timeout: Duration,
    self_logins: &'a [String],
}

async fn flush_channel(params: FlushParams<'_>) -> ChannelFlush {
    let FlushParams {
        log,
        through_seq,
        entries,
        n,
        batch_size,
        max_notes,
        merge_width,
        llm,
        model,
        timeout,
        self_logins,
    } = params;
    if *through_seq == 0 && log.through_fp.is_some() {
        *through_seq = rebind_through_seq(log, entries);
    }

    let eligible = eligible(entries, n as usize, *through_seq, self_logins);
    if !should_flush(&eligible, batch_size, entries) {
        return ChannelFlush::Idle;
    }

    let take = eligible.len().min(batch_size);
    let batch: Vec<_> = eligible.into_iter().take(take).collect();
    let user = batch
        .iter()
        .map(|e| format_batch_line(e, self_logins))
        .collect::<Vec<_>>()
        .join("\n");

    let body = match paraphrase(llm, model, timeout, user).await {
        Ok(body) => body,
        Err(_) => return ChannelFlush::Failed,
    };

    let last = batch.last().expect("non-empty batch");
    let identities = identities_from_batch(&batch);
    let through = last.seq;
    append_note(log, body, identities, through, last);
    *through_seq = through;

    if coarsen_if_needed(log, max_notes, merge_width, Some(llm), model, timeout)
        .await
        .is_err()
    {
        return ChannelFlush::Failed;
    }

    ChannelFlush::Saved
}

fn record_failure(state: &mut RecapState) {
    state.failures = state.failures.saturating_add(1);
    if state.failures >= RECAP_PARK_FAILURES {
        state.parked = true;
        tracing::warn!(
            failures = state.failures,
            "recap batcher parked after consecutive failures"
        );
    }
}

/// One batcher iteration: snapshot settings, optionally flush each channel.
pub(crate) async fn recap_batcher_tick(
    handle: &RecapHandle,
    settings: &SettingsHandle,
    llm: &dyn LlmClient,
    primary: &ChatHistory,
    ai: Option<&ChatHistory>,
    self_logins: &[String],
    last_seen_model: &mut Option<String>,
) {
    let snap = settings.load();
    let model = match snap.ai.history.recap_model.as_deref() {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => {
            *last_seen_model = None;
            return;
        }
    };

    if last_seen_model.as_deref() != Some(model.as_str()) {
        *last_seen_model = Some(model.clone());
        let mut guard = handle.inner.lock().await;
        guard.parked = false;
        guard.failures = 0;
    }

    let timeout = Duration::from_secs(snap.ai.connection.timeout);
    let n = snap.ai.history.n;
    let ai_n = snap.ai.history.ai_channel_n;
    let batch_size = snap.ai.history.batch_size as usize;
    let max_notes = snap.ai.history.max_notes as usize;
    let merge_width = snap.ai.history.merge_width as usize;
    drop(snap);

    let mut guard = handle.inner.lock().await;
    if guard.parked {
        return;
    }

    let primary_entries = primary.lock().await.snapshot();
    let mut primary_through = guard.primary_through_seq;
    match flush_channel(FlushParams {
        log: &mut guard.file.primary,
        through_seq: &mut primary_through,
        entries: &primary_entries,
        n,
        batch_size,
        max_notes,
        merge_width,
        llm,
        model: &model,
        timeout,
        self_logins,
    })
    .await
    {
        ChannelFlush::Idle => {}
        ChannelFlush::Failed => record_failure(&mut guard),
        ChannelFlush::Saved => {
            if let Err(err) =
                crate::util::persist::atomic_save_ron_async(&guard.file, &guard.path).await
            {
                tracing::error!(?err, path = %guard.path.display(), "chat-recap.ron save failed");
            }
            guard.failures = 0;
        }
    }
    guard.primary_through_seq = primary_through;

    if guard.parked {
        return;
    }

    if let Some(ai_hist) = ai {
        let ai_entries = ai_hist.lock().await.snapshot();
        let mut ai_through = guard.ai_through_seq;
        match flush_channel(FlushParams {
            log: &mut guard.file.ai_channel,
            through_seq: &mut ai_through,
            entries: &ai_entries,
            n: ai_n,
            batch_size,
            max_notes,
            merge_width,
            llm,
            model: &model,
            timeout,
            self_logins,
        })
        .await
        {
            ChannelFlush::Idle => {}
            ChannelFlush::Failed => record_failure(&mut guard),
            ChannelFlush::Saved => {
                if let Err(err) =
                    crate::util::persist::atomic_save_ron_async(&guard.file, &guard.path).await
                {
                    tracing::error!(?err, path = %guard.path.display(), "chat-recap.ron save failed");
                }
                guard.failures = 0;
            }
        }
        guard.ai_through_seq = ai_through;
    }
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
    loop {
        tokio::select! {
            () = shutdown.notified() => break,
            () = tokio::time::sleep(TICK_INTERVAL) => {
                recap_batcher_tick(
                    &handle,
                    &settings,
                    llm.as_ref(),
                    &primary,
                    ai.as_ref(),
                    &self_logins,
                    &mut last_seen_model,
                )
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

        for _ in 0..5 {
            recap_batcher_tick(
                &handle,
                &settings,
                &llm,
                &primary,
                None,
                &[],
                &mut last_seen,
            )
            .await;
        }

        assert!(handle.parked().await);
        assert_eq!(calls.load(Ordering::SeqCst), 5);

        recap_batcher_tick(
            &handle,
            &settings,
            &llm,
            &primary,
            None,
            &[],
            &mut last_seen,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn trim_note_body_still_used_by_batcher_path() {
        let long = "x".repeat(900);
        assert!(trim_note_body(&long).len() <= crate::ai::recap::RECAP_NOTE_BYTES);
    }
}
