use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ai::chat_history::ChatHistoryEntry;

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
    use crate::util::persist::atomic_save_ron_async;

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
