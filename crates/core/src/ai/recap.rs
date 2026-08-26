use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ai::chat_history::ChatHistoryEntry;
use crate::twitch::mouth::Agent;

pub const RECAP_NOTE_BYTES: usize = 768;
pub const RECAP_MAX_GENERATION: u32 = 2;
pub const RECAP_SHEET_NOTES: usize = 3;
pub const RECAP_SHEET_LOGINS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecapIdentity {
    pub login: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineFingerprint {
    pub timestamp: DateTime<Utc>,
    pub username: String,
    pub text: String,
}

impl LineFingerprint {
    pub fn from_entry(e: &ChatHistoryEntry) -> Self {
        Self {
            timestamp: e.timestamp,
            username: e.username.clone(),
            text: e.text.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecapNote {
    pub body: String,
    pub identities: Vec<RecapIdentity>,
    pub through_seq: u64,
    pub generation: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecapChannelLog {
    pub notes: Vec<RecapNote>,
    pub through_ts: Option<DateTime<Utc>>,
    pub through_fp: Option<LineFingerprint>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecapFile {
    pub primary: RecapChannelLog,
    pub ai_channel: RecapChannelLog,
}

pub fn recap_path(data_dir: &Path) -> PathBuf {
    data_dir.join("chat-recap.ron")
}

pub fn strip_quotes(s: &str) -> String {
    const PAIRS: &[(char, char)] = &[('"', '"'), ('„', '“'), ('»', '«'), ('‘', '’'), ('‚', '’')];
    const LEFTOVER: &[char] = &['"', '„', '“', '»', '«', '‘', '’', '‚'];

    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(&(_, close)) = PAIRS.iter().find(|&&(open, _)| chars[i] == open) {
            i += 1;
            while i < chars.len() && chars[i] != close {
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }

    out.chars().filter(|c| !LEFTOVER.contains(c)).collect()
}

pub fn trim_note_body(s: &str) -> String {
    let stripped = strip_quotes(s);
    match stripped.char_indices().nth(RECAP_NOTE_BYTES) {
        None => stripped,
        Some((i, _)) => stripped[..i].to_string(),
    }
}

/// Newest `max_notes` notes, chronological (oldest of that window first).
pub fn injected_notes(log: &RecapChannelLog, max_notes: usize) -> &[RecapNote] {
    let n = log.notes.len();
    let start = n.saturating_sub(max_notes);
    &log.notes[start..]
}

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
        (None, None) => Some(entries.last().expect("non-empty").seq + 1),
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

pub fn rebind_through_seq(log: &RecapChannelLog, entries: &[ChatHistoryEntry]) -> u64 {
    if entries.is_empty() {
        return 0;
    }
    if let Some(fp) = &log.through_fp
        && let Some(entry) = entries
            .iter()
            .find(|e| e.timestamp == fp.timestamp && e.username == fp.username && e.text == fp.text)
    {
        return entry.seq;
    }
    let Some(ts) = log.through_ts else {
        return 0;
    };
    entries
        .iter()
        .filter(|e| e.timestamp <= ts)
        .map(|e| e.seq)
        .max()
        .unwrap_or(0)
}

pub async fn load_recap_file(path: &Path) -> RecapFile {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => ron::from_str(&raw).unwrap_or_else(|e| {
            tracing::error!(error = ?e, path = %path.display(), "chat-recap.ron unreadable, starting empty");
            RecapFile::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RecapFile::default(),
        Err(e) => {
            tracing::error!(error = ?e, path = %path.display(), "chat-recap.ron read failed, starting empty");
            RecapFile::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::chat_history::ChatHistorySource;
    use crate::util::persist::atomic_save_ron_async;

    fn e(seq: u64, user: &str, agent: Option<Agent>) -> ChatHistoryEntry {
        ChatHistoryEntry {
            seq,
            username: user.into(),
            display_name: None,
            user_id: None,
            text: format!("m{seq}"),
            source: if agent.is_some() {
                ChatHistorySource::Bot
            } else {
                ChatHistorySource::User
            },
            agent,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn cut_is_older_of_two_agent_windows() {
        // newest last: v1, aurora, v2, grok, v3  (n=2)
        let buf = vec![
            e(1, "v1", None),
            e(2, "aurora", Some(Agent::Aurora)),
            e(3, "v2", None),
            e(4, "grok", Some(Agent::Grok)),
            e(5, "v3", None),
        ];
        let cut = eligibility_cut_seq(&buf, 2, &[]).unwrap();
        let aurora_win = n_not_own(&buf, 2, Agent::Aurora, &[]);
        let grok_win = n_not_own(&buf, 2, Agent::Grok, &[]);
        let expect = aurora_win
            .iter()
            .chain(grok_win.iter())
            .map(|x| x.seq)
            .min()
            .unwrap();
        assert_eq!(cut, expect);
    }

    #[test]
    fn viewer_inside_both_tails_is_not_eligible() {
        let buf = vec![e(1, "old", None), e(2, "a", None), e(3, "b", None)];
        let through = 0;
        let el = eligible(&buf, 2, through, &[]);
        assert!(el.iter().all(|x| x.username != "a" && x.username != "b"));
        assert_eq!(el.len(), 1);
        assert_eq!(el[0].username, "old");
    }

    #[test]
    fn rebind_matches_fingerprint_not_old_seq() {
        let fp = LineFingerprint {
            timestamp: Utc::now(),
            username: "alice".into(),
            text: "hi".into(),
        };
        let log = RecapChannelLog {
            notes: vec![],
            through_ts: Some(fp.timestamp),
            through_fp: Some(fp.clone()),
        };
        let mut alice = e(1, "alice", None);
        alice.timestamp = fp.timestamp;
        alice.text = "hi".into();
        let bob = e(2, "bob", None);
        assert_eq!(rebind_through_seq(&log, &[alice, bob]), 1);
    }

    #[test]
    fn rebind_falls_back_to_through_ts_when_fp_missing() {
        let ts = Utc::now() - chrono::Duration::hours(1);
        let log = RecapChannelLog {
            notes: vec![],
            through_ts: Some(ts),
            through_fp: Some(LineFingerprint {
                timestamp: ts,
                username: "gone".into(),
                text: "bye".into(),
            }),
        };
        let mut old = e(1, "x", None);
        old.timestamp = ts - chrono::Duration::minutes(1);
        let mut newer = e(2, "y", None);
        newer.timestamp = ts + chrono::Duration::minutes(1);
        assert_eq!(rebind_through_seq(&log, &[old, newer]), 1);
    }

    #[test]
    fn strip_quotes_removes_german_spans() {
        let out = strip_quotes("Aurora sagte „das ist so washed“ zu bob");
        assert!(!out.contains('„') && !out.contains('“'));
        assert!(!out.contains("das ist so washed"));
    }

    #[test]
    fn strip_quotes_drops_ascii_spans() {
        let out = strip_quotes(r#"bob rief "sigma grindset""#);
        assert!(!out.contains('"'));
    }

    #[test]
    fn injected_notes_keeps_newest_max() {
        let log = RecapChannelLog {
            notes: (0..5)
                .map(|i| RecapNote {
                    body: format!("n{i}"),
                    identities: vec![],
                    through_seq: i,
                    generation: 0,
                })
                .collect(),
            ..Default::default()
        };
        let inj = injected_notes(&log, 3);
        assert_eq!(inj.len(), 3);
        assert_eq!(inj[0].body, "n2");
        assert_eq!(inj[2].body, "n4");
    }

    #[tokio::test]
    async fn persist_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = recap_path(dir.path());
        let file = RecapFile {
            primary: RecapChannelLog {
                notes: vec![RecapNote {
                    body: "raid vorbei".into(),
                    identities: vec![RecapIdentity {
                        login: "alice".into(),
                        user_id: Some("1".into()),
                    }],
                    through_seq: 9,
                    generation: 0,
                }],
                through_ts: Some(Utc::now()),
                through_fp: Some(LineFingerprint {
                    timestamp: Utc::now(),
                    username: "alice".into(),
                    text: "hi".into(),
                }),
            },
            ai_channel: RecapChannelLog::default(),
        };
        atomic_save_ron_async(&file, &path).await.unwrap();
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let back: RecapFile = ron::from_str(&raw).unwrap();
        assert_eq!(back.primary.notes[0].body, "raid vorbei");
    }
}
