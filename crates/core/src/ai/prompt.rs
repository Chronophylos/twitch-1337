//! Prompt-file token substitution and recent-chat rendering for the `!ai` turn.
//!
//! Relocated out of the old `ai/memory/inject.rs` when memory moved to mem0.
//! Only the parts the live turn still needs survive here: prompt-template
//! substitution and the rolling recent-chat sections. Memory-file injection
//! lives (for now) in the old module, which the web editor still uses.

use chrono_tz::Europe::Berlin;

use crate::ai::chat_history::{ChatHistoryBuffer, ChatHistoryEntry, ChatHistorySource};

/// Per-section byte caps for rolling chat injected into the prompt.
const RECENT_CHAT_PRIMARY_BYTES: usize = 2048;
const RECENT_CHAT_AI_CHANNEL_BYTES: usize = 1024;

#[derive(Clone, Copy)]
pub struct SubstitutionVars<'a> {
    pub speaker_username: &'a str,
    pub speaker_display: &'a str,
    pub speaker_user_id: &'a str,
    pub speaker_role: &'a str,
    pub channel: &'a str,
    pub date: &'a str,
    pub model: &'a str,
    pub model_id: &'a str,
}

pub fn substitute(template: &str, v: SubstitutionVars<'_>) -> String {
    template
        .replace("{speaker_username}", v.speaker_username)
        .replace("{speaker_display}", v.speaker_display)
        .replace("{speaker_user_id}", v.speaker_user_id)
        .replace("{speaker_role}", v.speaker_role)
        .replace("{channel}", v.channel)
        .replace("{date}", v.date)
        .replace("{model}", v.model)
        .replace("{model_id}", v.model_id)
}

struct RenderedRecentSection {
    body: String,
}

/// Render one `## Recent chat (#login)` section, newest-first up to `cap` bytes,
/// then reverse to chronological order. Returns `None` for missing or empty
/// buffers.
async fn render_recent_section(
    buf: Option<&std::sync::Arc<tokio::sync::Mutex<ChatHistoryBuffer>>>,
    login: &str,
    cap: usize,
    bot_login: &str,
    persona_name: &str,
) -> Option<RenderedRecentSection> {
    let buf = buf?;
    let snapshot: Vec<ChatHistoryEntry> = buf.lock().await.snapshot();
    if snapshot.is_empty() {
        return None;
    }

    // ponytail: scoping moved to mem0, no per-line username tracking.
    let mut chosen: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for entry in snapshot.iter().rev() {
        let line = format_entry_line(entry, bot_login, persona_name);
        let line_bytes = line.len() + 1;
        if bytes + line_bytes > cap {
            break;
        }
        bytes += line_bytes;
        chosen.push(line);
    }
    if chosen.is_empty() {
        return None;
    }
    chosen.reverse();

    let mut body = format!("## Recent chat (#{login})\n");
    body.push_str(&chosen.join("\n"));
    Some(RenderedRecentSection { body })
}

fn format_entry_line(entry: &ChatHistoryEntry, bot_login: &str, persona_name: &str) -> String {
    let ts = entry.timestamp.with_timezone(&Berlin).format("%H:%M");
    let is_self =
        entry.source == ChatHistorySource::Bot || entry.username.eq_ignore_ascii_case(bot_login);
    if is_self {
        return format!("[{ts}] {persona_name} (self): {}", entry.text);
    }
    let name = entry
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(entry.username.as_str());
    format!("[{ts}] {name}: {}", entry.text)
}

pub struct RecentChatOpts {
    pub primary_history:
        Option<std::sync::Arc<tokio::sync::Mutex<crate::ai::chat_history::ChatHistoryBuffer>>>,
    pub primary_login: String,
    pub ai_channel_history:
        Option<std::sync::Arc<tokio::sync::Mutex<crate::ai::chat_history::ChatHistoryBuffer>>>,
    pub ai_channel_login: Option<String>,
    pub invoked_in_ai_channel: bool,
    pub bot_login: String,
    pub persona_name: String,
}

/// Joined `## Recent chat (#login)` sections, the invoking channel first. Empty when no history.
pub async fn render_recent_chat(opts: RecentChatOpts) -> String {
    let primary = render_recent_section(
        opts.primary_history.as_ref(),
        &opts.primary_login,
        RECENT_CHAT_PRIMARY_BYTES,
        &opts.bot_login,
        &opts.persona_name,
    )
    .await;
    let ai = match (
        opts.ai_channel_history.as_ref(),
        opts.ai_channel_login.as_ref(),
    ) {
        (Some(b), Some(l)) => {
            render_recent_section(
                Some(b),
                l,
                RECENT_CHAT_AI_CHANNEL_BYTES,
                &opts.bot_login,
                &opts.persona_name,
            )
            .await
        }
        _ => None,
    };
    let (first, second) = if opts.invoked_in_ai_channel {
        (ai, primary)
    } else {
        (primary, ai)
    };
    [first, second]
        .into_iter()
        .flatten()
        .map(|s| s.body)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_entry_line_self_uses_persona_and_self_tag() {
        let entry = ChatHistoryEntry {
            seq: 1,
            username: "chronophylosbot".into(),
            display_name: Some("Aurora".into()),
            user_id: None,
            text: "gemerkt".into(),
            source: ChatHistorySource::Bot,
            timestamp: chrono::Utc::now(),
        };
        let line = format_entry_line(&entry, "chronophylosbot", "Aurora");
        assert!(line.ends_with(" Aurora (self): gemerkt"), "got: {line}");
    }

    #[test]
    fn format_entry_line_other_uses_display_name() {
        let entry = ChatHistoryEntry {
            seq: 1,
            username: "magie_023".into(),
            display_name: Some("MagieDisplay".into()),
            user_id: Some("141690010".into()),
            text: "hi".into(),
            source: ChatHistorySource::User,
            timestamp: chrono::Utc::now(),
        };
        let line = format_entry_line(&entry, "chronophylosbot", "Aurora");
        assert!(line.ends_with(" MagieDisplay: hi"), "got: {line}");
    }

    #[test]
    fn format_entry_line_other_falls_back_to_username_when_no_display() {
        let entry = ChatHistoryEntry {
            seq: 1,
            username: "lurker42".into(),
            display_name: None,
            user_id: None,
            text: "?".into(),
            source: ChatHistorySource::User,
            timestamp: chrono::Utc::now(),
        };
        let line = format_entry_line(&entry, "chronophylosbot", "Aurora");
        assert!(line.ends_with(" lurker42: ?"), "got: {line}");
    }
}
