//! Rolling chat-history rendering for the `## Recent chat (#login)` section.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono_tz::Europe::Berlin;
use tokio::sync::Mutex;

use crate::ai::chat_history::{ChatHistoryBuffer, ChatHistoryEntry};
use crate::ai::recap::{eligible, is_own, n_not_own};
use crate::twitch::mouth::Agent;

use super::{BuildOpts, InvocationChannel};

/// Rendered rolling chat for one turn, plus the lowercased logins that survived.
/// Shared by the Aurora and grok context builders.
pub(super) struct RecentChat {
    pub(super) body: String,
    pub(super) mentioned: std::collections::BTreeSet<String>,
}

pub(super) async fn render_recent_chat(opts: &BuildOpts) -> RecentChat {
    let primary_rendered = render_recent_section(
        opts.primary_history.as_ref(),
        &opts.primary_login,
        opts.n,
        opts.recap_through_seq,
        opts,
    )
    .await;
    let ai_rendered = match (
        opts.ai_channel_history.as_ref(),
        opts.ai_channel_login.as_ref(),
    ) {
        (Some(buf), Some(login)) => {
            render_recent_section(
                Some(buf),
                login,
                opts.ai_channel_n,
                opts.ai_channel_recap_through_seq,
                opts,
            )
            .await
        }
        _ => None,
    };

    let (first, second) = match opts.invocation_channel {
        InvocationChannel::AiChannel => (ai_rendered, primary_rendered),
        InvocationChannel::Primary => (primary_rendered, ai_rendered),
    };
    let mut mentioned: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut recent_sections: Vec<String> = Vec::with_capacity(2);
    for section in [first, second].into_iter().flatten() {
        for u in section.usernames {
            mentioned.insert(u);
        }
        recent_sections.push(section.body);
    }

    RecentChat {
        body: recent_sections.join("\n\n"),
        mentioned,
    }
}

/// Packing tier for character sheets: speaker (0), chat-window (1), recap-only (2).
pub(super) fn user_pack_tier(
    login_lc: &str,
    speaker_lc: &str,
    mentioned: &BTreeSet<String>,
    recap_sheet_logins: &BTreeSet<String>,
) -> u8 {
    if !speaker_lc.is_empty() && login_lc == speaker_lc {
        0
    } else if mentioned.contains(login_lc) {
        1
    } else if recap_sheet_logins.contains(login_lc) {
        2
    } else {
        3
    }
}

struct RenderedRecentSection {
    pub(super) body: String,
    usernames: Vec<String>,
}

/// Render one `## Recent chat (#login)` section from the N-not-own window plus
/// the latest own line, with optional Recap stretch. Returns the lowercased
/// usernames of every line that survived. Returns `None` for missing or empty
/// buffers.
async fn render_recent_section(
    buf: Option<&Arc<Mutex<ChatHistoryBuffer>>>,
    login: &str,
    n: usize,
    recap_through_seq: u64,
    opts: &BuildOpts,
) -> Option<RenderedRecentSection> {
    let buf = buf?;
    let snapshot: Vec<ChatHistoryEntry> = buf.lock().await.snapshot();
    if snapshot.is_empty() {
        return None;
    }

    let current_agent = opts.current_agent;
    let self_logins = &opts.self_logins;
    let batch_size = opts.batch_size;

    let mut others: Vec<&ChatHistoryEntry> = n_not_own(&snapshot, n, current_agent, self_logins);

    if opts.recap_enabled {
        let stretch = eligible(&snapshot, n, recap_through_seq, self_logins);
        let mut seen: std::collections::BTreeSet<u64> = others.iter().map(|e| e.seq).collect();
        for entry in stretch {
            if is_own(entry, current_agent, self_logins) {
                continue;
            }
            if seen.insert(entry.seq) {
                others.push(entry);
            }
        }
        others.sort_by_key(|e| e.seq);
        let cap = n + batch_size;
        if others.len() > cap {
            let keep = cap;
            let drop = others.len() - keep;
            others.drain(..drop);
        }
    }

    let mut chosen: Vec<&ChatHistoryEntry> = others;
    let chosen_seqs: std::collections::BTreeSet<u64> = chosen.iter().map(|e| e.seq).collect();

    if let Some(latest_own) = snapshot
        .iter()
        .rev()
        .find(|e| is_own(e, current_agent, self_logins))
        && !chosen_seqs.contains(&latest_own.seq)
    {
        chosen.push(latest_own);
    }

    chosen.sort_by_key(|e| e.seq);

    if chosen.is_empty() {
        return None;
    }

    let mut usernames: Vec<String> = Vec::with_capacity(chosen.len());
    let mut lines: Vec<String> = Vec::with_capacity(chosen.len());
    for entry in chosen {
        lines.push(format_entry_line(entry, current_agent, self_logins));
        usernames.push(entry.username.to_ascii_lowercase());
    }

    let mut body = format!("## Recent chat (#{login})\n");
    body.push_str(&lines.join("\n"));
    Some(RenderedRecentSection { body, usernames })
}

pub(crate) fn format_entry_line(
    entry: &ChatHistoryEntry,
    current_agent: Agent,
    self_logins: &[String],
) -> String {
    let ts = entry.timestamp.with_timezone(&Berlin).format("%H:%M");
    let name = entry
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(entry.username.as_str());
    // Tagged rows are authoritative. Untagged ones only come from prefill, where
    // the speaking agent was never recorded, so fall back to the login.
    let is_self = match entry.agent {
        Some(agent) => agent == current_agent,
        None => self_logins
            .iter()
            .any(|l| l.eq_ignore_ascii_case(&entry.username)),
    };
    if is_self {
        return format!("[{ts}] {name} (self): {}", entry.text);
    }
    format!("[{ts}] {name}: {}", entry.text)
}
