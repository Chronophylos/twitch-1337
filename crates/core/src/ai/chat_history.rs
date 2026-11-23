use std::collections::VecDeque;
use std::sync::Arc;

use jiff::Timestamp;
use serde::Serialize;

use crate::settings::SettingsHandle;
use crate::twitch::mouth::Agent;

pub const MAX_TOOL_RESULT_MESSAGES: usize = 200;
const DEFAULT_TOOL_RESULT_MESSAGES: usize = 50;

pub type ChatHistory = Arc<tokio::sync::Mutex<ChatHistoryBuffer>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatHistorySource {
    User,
    Bot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatHistoryEntry {
    pub seq: u64,
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub text: String,
    pub source: ChatHistorySource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
    pub timestamp: Timestamp,
}

#[derive(Debug)]
pub struct ChatHistoryBuffer {
    settings: SettingsHandle,
    next_seq: u64,
    entries: VecDeque<ChatHistoryEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatHistoryQuery {
    pub limit: Option<usize>,
    pub user: Option<String>,
    pub contains: Option<String>,
    pub before_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatHistoryPage {
    pub messages: Vec<ChatHistoryEntry>,
    pub has_more: bool,
    pub next_before_seq: Option<u64>,
}

impl ChatHistoryBuffer {
    pub fn new(settings: SettingsHandle) -> Self {
        Self {
            settings,
            next_seq: 1,
            entries: VecDeque::new(),
        }
    }

    pub fn from_prefill<I>(settings: SettingsHandle, messages: I) -> Self
    where
        I: IntoIterator<Item = (String, String, Timestamp)>,
    {
        let mut buf = Self::new(settings);
        let cap = buf.current_capacity();
        let mut items: Vec<(String, String, Timestamp)> = messages.into_iter().collect();
        if items.len() > cap {
            items.drain(..items.len() - cap);
        }
        for (username, text, timestamp) in items {
            buf.push_user_at(username, text, timestamp);
        }
        buf
    }

    /// Live capacity: `ai.history.length` from the current settings snapshot.
    fn current_capacity(&self) -> usize {
        self.settings.load().ai.history.length as usize
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push_user(&mut self, username: impl Into<String>, text: impl Into<String>) {
        self.push_user_at(username, text, Timestamp::now());
    }

    pub fn push_user_at(
        &mut self,
        username: impl Into<String>,
        text: impl Into<String>,
        timestamp: Timestamp,
    ) {
        self.push(
            username,
            None,
            None,
            text,
            ChatHistorySource::User,
            None,
            timestamp,
        );
    }

    pub fn push_user_with_identity(
        &mut self,
        username: impl Into<String>,
        display_name: Option<&str>,
        user_id: Option<&str>,
        text: impl Into<String>,
    ) {
        self.push_user_with_identity_at(username, display_name, user_id, text, Timestamp::now());
    }

    pub fn push_user_with_identity_at(
        &mut self,
        username: impl Into<String>,
        display_name: Option<&str>,
        user_id: Option<&str>,
        text: impl Into<String>,
        timestamp: Timestamp,
    ) {
        self.push(
            username,
            display_name.map(str::to_string),
            user_id.map(str::to_string),
            text,
            ChatHistorySource::User,
            None,
            timestamp,
        );
    }

    pub fn push_bot(&mut self, username: impl Into<String>, text: impl Into<String>, agent: Agent) {
        self.push_bot_at(username, text, agent, Timestamp::now());
    }

    pub fn push_bot_at(
        &mut self,
        username: impl Into<String>,
        text: impl Into<String>,
        agent: Agent,
        timestamp: Timestamp,
    ) {
        self.push(
            username,
            None,
            None,
            text,
            ChatHistorySource::Bot,
            Some(agent),
            timestamp,
        );
    }

    pub fn push_bot_with_identity(
        &mut self,
        username: impl Into<String>,
        display_name: Option<&str>,
        agent: Agent,
        text: impl Into<String>,
    ) {
        self.push_bot_with_identity_at(username, display_name, agent, text, Timestamp::now());
    }

    pub fn push_bot_with_identity_at(
        &mut self,
        username: impl Into<String>,
        display_name: Option<&str>,
        agent: Agent,
        text: impl Into<String>,
        timestamp: Timestamp,
    ) {
        self.push(
            username,
            display_name.map(str::to_string),
            None,
            text,
            ChatHistorySource::Bot,
            Some(agent),
            timestamp,
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "private helper, arity mirrors ChatHistoryEntry's fields 1:1"
    )]
    fn push(
        &mut self,
        username: impl Into<String>,
        display_name: Option<String>,
        user_id: Option<String>,
        text: impl Into<String>,
        source: ChatHistorySource,
        agent: Option<Agent>,
        timestamp: Timestamp,
    ) {
        let cap = self.current_capacity();
        if cap == 0 {
            return;
        }
        while self.entries.len() >= cap {
            self.entries.pop_front();
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.entries.push_back(ChatHistoryEntry {
            seq,
            username: username.into(),
            display_name,
            user_id,
            text: text.into(),
            source,
            agent,
            timestamp,
        });
    }

    pub fn snapshot(&self) -> Vec<ChatHistoryEntry> {
        self.entries.iter().cloned().collect()
    }

    pub fn query(&self, query: ChatHistoryQuery) -> ChatHistoryPage {
        let limit = query
            .limit
            .unwrap_or(DEFAULT_TOOL_RESULT_MESSAGES)
            .clamp(1, MAX_TOOL_RESULT_MESSAGES);
        let user = query
            .user
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase);
        let contains = query
            .contains
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase);

        let mut newest_first = Vec::with_capacity(limit.saturating_add(1));
        for entry in self.entries.iter().rev() {
            if query.before_seq.is_some_and(|seq| entry.seq >= seq) {
                continue;
            }
            if let Some(ref user) = user
                && entry.username.to_ascii_lowercase() != *user
            {
                continue;
            }
            if let Some(ref needle) = contains
                && !entry.text.to_ascii_lowercase().contains(needle)
            {
                continue;
            }

            newest_first.push(entry.clone());
            if newest_first.len() > limit {
                break;
            }
        }

        let has_more = newest_first.len() > limit;
        if has_more {
            newest_first.truncate(limit);
        }
        newest_first.reverse();

        let next_before_seq = if has_more {
            newest_first.first().map(|entry| entry.seq)
        } else {
            None
        };
        ChatHistoryPage {
            messages: newest_first,
            has_more,
            next_before_seq,
        }
    }
}

// Recap-eligibility windows over a history snapshot. Shared by the Recap
// batcher and the Recent Chat renderer, so they live with the entry type
// rather than in either consumer.
pub fn is_own(entry: &ChatHistoryEntry, agent: Agent, self_logins: &[String]) -> bool {
    match entry.agent {
        Some(entry_agent) => entry_agent == agent,
        None => self_logins
            .iter()
            .any(|login| login.eq_ignore_ascii_case(&entry.username)),
    }
}

/// Newest `n` lines that are not this agent's own, oldest-first.
pub fn n_not_own<'a>(
    entries: &'a [ChatHistoryEntry],
    n: usize,
    agent: Agent,
    self_logins: &[String],
) -> Vec<&'a ChatHistoryEntry> {
    let mut picked: Vec<&ChatHistoryEntry> = Vec::with_capacity(n);
    for entry in entries.iter().rev() {
        if is_own(entry, agent, self_logins) {
            continue;
        }
        picked.push(entry);
        if picked.len() == n {
            break;
        }
    }
    picked.reverse();
    picked
}

/// Oldest seq among Aurora's N-not-own window and Grok's, whichever is older
/// (smaller seq). `None` if the buffer is empty.
pub fn eligibility_cut_seq(
    entries: &[ChatHistoryEntry],
    n: usize,
    self_logins: &[String],
) -> Option<u64> {
    if entries.is_empty() {
        return None;
    }
    let aurora_oldest = n_not_own(entries, n, Agent::Aurora, self_logins)
        .first()
        .map(|e| e.seq);
    let grok_oldest = n_not_own(entries, n, Agent::Grok, self_logins)
        .first()
        .map(|e| e.seq);
    match (aurora_oldest, grok_oldest) {
        (Some(a), Some(g)) => Some(a.min(g)),
        (Some(a), None) => Some(a),
        (None, Some(g)) => Some(g),
        (None, None) => entries.last().map(|e| e.seq + 1),
    }
}

pub fn eligible<'a>(
    entries: &'a [ChatHistoryEntry],
    n: usize,
    through_seq: u64,
    self_logins: &[String],
) -> Vec<&'a ChatHistoryEntry> {
    let Some(cut) = eligibility_cut_seq(entries, n, self_logins) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|e| e.seq < cut && e.seq > through_seq)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::settings::Settings;

    /// Build a `SettingsHandle` whose `ai.history.length` is set to `cap`.
    fn make_handle(cap: u64) -> SettingsHandle {
        let mut s = Settings::compiled_defaults();
        s.ai.history.length = cap;
        SettingsHandle::from_settings(s)
    }

    fn sample_buffer() -> ChatHistoryBuffer {
        let handle = make_handle(10);
        let mut buffer = ChatHistoryBuffer::new(handle);
        buffer.push_user("alice", "hello chat");
        buffer.push_user("bob", "weather is bad");
        buffer.push_user("alice", "weather got better");
        buffer.push_bot("bot", "I can check that", Agent::Aurora);
        buffer.push_user("carol", "different topic");
        buffer
    }

    #[test]
    fn query_returns_recent_messages_chronologically() {
        let buffer = sample_buffer();
        let page = buffer.query(ChatHistoryQuery {
            limit: Some(3),
            ..ChatHistoryQuery::default()
        });

        assert_eq!(
            page.messages
                .iter()
                .map(|entry| entry.username.as_str())
                .collect::<Vec<_>>(),
            vec!["alice", "bot", "carol"]
        );
        assert!(page.has_more);
        assert_eq!(page.next_before_seq, Some(3));
    }

    #[test]
    fn query_filters_by_user_and_contains() {
        let buffer = sample_buffer();
        let page = buffer.query(ChatHistoryQuery {
            limit: Some(10),
            user: Some("ALICE".into()),
            contains: Some("WEATHER".into()),
            before_seq: None,
        });

        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].seq, 3);
        assert_eq!(page.messages[0].text, "weather got better");
        assert!(!page.has_more);
    }

    #[test]
    fn query_pages_with_before_seq() {
        let buffer = sample_buffer();
        let page = buffer.query(ChatHistoryQuery {
            limit: Some(2),
            before_seq: Some(4),
            ..ChatHistoryQuery::default()
        });

        assert_eq!(
            page.messages
                .iter()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(page.has_more);
        assert_eq!(page.next_before_seq, Some(2));
    }

    #[test]
    fn query_clamps_limit_to_tool_maximum() {
        let handle = make_handle(250);
        let mut buffer = ChatHistoryBuffer::new(handle);
        for i in 0..250 {
            buffer.push_user("alice", format!("message {i}"));
        }

        let page = buffer.query(ChatHistoryQuery {
            limit: Some(500),
            ..ChatHistoryQuery::default()
        });

        assert_eq!(page.messages.len(), MAX_TOOL_RESULT_MESSAGES);
        assert!(page.has_more);
        assert_eq!(page.messages[0].seq, 51);
    }

    #[test]
    fn from_prefill_assigns_sequence_numbers_and_marks_user_source() {
        let handle = make_handle(2);
        let buffer = ChatHistoryBuffer::from_prefill(
            handle,
            vec![
                ("alice".to_string(), "one".to_string(), Timestamp::now()),
                ("bob".to_string(), "two".to_string(), Timestamp::now()),
                ("carol".to_string(), "three".to_string(), Timestamp::now()),
            ],
        );

        let page = buffer.query(ChatHistoryQuery {
            limit: Some(10),
            ..ChatHistoryQuery::default()
        });
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[0].seq, 1);
        assert_eq!(page.messages[0].username, "bob");
        assert_eq!(page.messages[1].seq, 2);
        assert_eq!(page.messages[1].source, ChatHistorySource::User);
    }

    /// Demonstrates live capacity rebinding: fill the buffer, halve the capacity
    /// via the settings handle, push one more entry, observe the buffer trimmed
    /// to the new cap.
    #[test]
    fn live_capacity_rebind_trims_on_next_push() {
        let handle = make_handle(6);
        let mut buffer = ChatHistoryBuffer::new(handle.clone());

        // Fill to capacity.
        for i in 1..=6 {
            buffer.push_user("alice", format!("msg {i}"));
        }
        assert_eq!(buffer.len(), 6);

        // Halve the capacity through the settings handle.
        let mut new_settings = Settings::compiled_defaults();
        new_settings.ai.history.length = 3;
        handle.store(Arc::new(new_settings));

        // Push one more — the buffer should trim to the new cap (3) and then add
        // the new entry, ending up with exactly 3 entries.
        buffer.push_user("alice", "trigger trim");
        assert_eq!(buffer.len(), 3);

        let snap = buffer.snapshot();
        assert_eq!(snap.last().unwrap().text, "trigger trim");
    }
}
