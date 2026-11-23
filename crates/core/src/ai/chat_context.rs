//! The shared primary chat-history buffer, built once at composition and
//! cloned by every consumer (`!ai`, `!news`, `!haiku`, history recording,
//! the recap batcher).

use std::sync::Arc;

use crate::ai::chat_history::{ChatHistory, ChatHistoryBuffer};
use crate::ai::recap::RecapHandle;
use crate::settings::SettingsHandle;

/// Chat history buffer and channel login for `!ai`.
#[derive(Clone)]
pub struct ChatContext {
    pub primary_history: ChatHistory,
    pub primary_login: String,
}

impl ChatContext {
    /// Build the shared chat-history buffer once at composition (main /
    /// `TestBotBuilder`). Every consumer — `!ai`, `!news`, `!haiku`, the
    /// dispatcher's history recording, and the recap batcher — clones the
    /// same buffer out of `Services` instead of relaying it through handler
    /// configs.
    ///
    /// Returns `None` when AI is disabled or chat history is off. Rebinds
    /// `recap` to the freshly built buffer so a restart resumes the recap
    /// without a gap.
    pub async fn build(
        settings: &SettingsHandle,
        ai_enabled: bool,
        channel: &str,
        recap: Option<&RecapHandle>,
    ) -> Option<ChatContext> {
        let snapshot = settings.load();
        if !(ai_enabled && snapshot.ai.history.length > 0) {
            return None;
        }
        let history_length = snapshot.ai.history.length as usize;
        let prefill_cfg = snapshot
            .ai
            .prefill
            .as_ref()
            .map(super::prefill::HistoryPrefillConfig::from);
        let buffer = if let Some(cfg) = &prefill_cfg {
            tracing::info!("ai.prefill configured; fetching history before startup continues");
            let prefilled =
                super::prefill::prefill_chat_history(channel, history_length, cfg).await;
            ChatHistoryBuffer::from_prefill(settings.clone(), prefilled)
        } else {
            ChatHistoryBuffer::new(settings.clone())
        };
        let primary_history: ChatHistory = Arc::new(tokio::sync::Mutex::new(buffer));

        if let Some(handle) = recap {
            let entries = primary_history.lock().await.snapshot();
            handle.rebind_primary(&entries).await;
        }

        Some(ChatContext {
            primary_history,
            primary_login: channel.to_owned(),
        })
    }
}
