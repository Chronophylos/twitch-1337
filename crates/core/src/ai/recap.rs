use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::ai::chat_history::{ChatHistoryEntry, ChatHistorySource};
use crate::ai::memory::inject::render_recap_section;
use crate::twitch::mouth::Agent;

pub const RECAP_NOTE_BYTES: usize = 768;
pub const RECAP_MAX_GENERATION: u32 = 2;
pub const RECAP_SHEET_NOTES: usize = 3;
pub const RECAP_SHEET_LOGINS: usize = 16;
/// Quiet-channel flush: leftover eligible lines recap after the newest buffer
/// line is this old. Must outlast typical stream lulls (BRB, raid, talking to
/// chat) or partial batches fire every pause. Not a setting — `batch_size` is
/// the live knob; eviction backpressure still drains before the raw window
/// drops uncovered lines.
pub const RECAP_FLUSH_AGE: SignedDuration = SignedDuration::from_mins(30);
pub const RECAP_PARK_FAILURES: u32 = 5;
pub const RECAP_BACKOFF_INITIAL: Duration = Duration::from_secs(2);
pub const RECAP_BACKOFF_CAP: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecapIdentity {
    pub login: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineFingerprint {
    pub timestamp: Timestamp,
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
    pub through_ts: Option<Timestamp>,
    pub through_fp: Option<LineFingerprint>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecapFile {
    pub primary: RecapChannelLog,
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
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == close) {
                i += end + 2;
            } else {
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }

    out.chars().filter(|c| !LEFTOVER.contains(c)).collect()
}

/// Strips quoted spans, then caps the body at [`RECAP_NOTE_BYTES`] bytes,
/// cutting at the last char boundary that fits.
pub fn trim_note_body(s: &str) -> String {
    let mut stripped = strip_quotes(s);
    stripped.truncate(stripped.floor_char_boundary(RECAP_NOTE_BYTES));
    stripped
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

pub struct RecapState {
    pub file: RecapFile,
    pub path: PathBuf,
    pub parked: bool,
    pub failures: u32,
    pub primary_through_seq: u64,
}

pub struct RecapHandle {
    pub(crate) inner: Arc<Mutex<RecapState>>,
}

impl Clone for RecapHandle {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl RecapHandle {
    pub async fn load(data_dir: PathBuf) -> Self {
        let path = recap_path(&data_dir);
        let file = load_recap_file(&path).await;
        Self {
            inner: Arc::new(Mutex::new(RecapState {
                file,
                path,
                parked: false,
                failures: 0,
                primary_through_seq: 0,
            })),
        }
    }

    pub async fn parked(&self) -> bool {
        self.inner.lock().await.parked
    }

    pub async fn sheet_logins(&self) -> BTreeSet<String> {
        let guard = self.inner.lock().await;
        let log = &guard.file.primary;
        let n = log.notes.len();
        let start = n.saturating_sub(RECAP_SHEET_NOTES);
        let mut out = BTreeSet::new();
        for note in log.notes[start..].iter().rev() {
            for id in &note.identities {
                out.insert(id.login.to_ascii_lowercase());
                if out.len() >= RECAP_SHEET_LOGINS {
                    return out;
                }
            }
        }
        out
    }

    pub async fn primary_through_seq(&self) -> u64 {
        self.inner.lock().await.primary_through_seq
    }

    /// Fenced Recap block for `channel_login`, built from the newest
    /// `max_notes` notes. Empty when there are none.
    pub async fn render_fenced(
        &self,
        channel_login: &str,
        max_notes: usize,
        nonce: &str,
    ) -> String {
        let guard = self.inner.lock().await;
        render_recap_section(
            channel_login,
            injected_notes(&guard.file.primary, max_notes),
            nonce,
        )
    }

    pub async fn rebind_primary(&self, entries: &[ChatHistoryEntry]) {
        let mut guard = self.inner.lock().await;
        guard.primary_through_seq = rebind_through_seq(&guard.file.primary, entries);
    }
}

pub fn format_batch_line(entry: &ChatHistoryEntry, self_logins: &[String]) -> String {
    let ts = crate::util::to_berlin(entry.timestamp).strftime("%H:%M");
    let name = match entry.agent {
        Some(Agent::Aurora) => "Aurora",
        Some(Agent::Grok) => "Grok",
        Some(Agent::Utility) => "bot",
        None if entry.source == ChatHistorySource::Bot
            || self_logins
                .iter()
                .any(|l| l.eq_ignore_ascii_case(&entry.username)) =>
        {
            "bot"
        }
        None => entry
            .display_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(entry.username.as_str()),
    };
    format!("[{ts}] {name}: {}", entry.text)
}

pub fn identities_from_batch(entries: &[&ChatHistoryEntry]) -> Vec<RecapIdentity> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        let key = entry.username.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(RecapIdentity {
                login: entry.username.clone(),
                user_id: entry.user_id.clone(),
            });
        }
    }
    out
}

pub fn append_note(
    log: &mut RecapChannelLog,
    body: String,
    identities: Vec<RecapIdentity>,
    through_seq: u64,
    last_entry: &ChatHistoryEntry,
) {
    log.notes.push(RecapNote {
        body: trim_note_body(&body),
        identities,
        through_seq,
        generation: 0,
    });
    log.through_ts = Some(last_entry.timestamp);
    log.through_fp = Some(LineFingerprint::from_entry(last_entry));
}

pub async fn load_recap_file(path: &Path) -> RecapFile {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => ron::from_str(&raw).unwrap_or_else(|e| {
            crate::log_handled!(error, e, path = %path.display(), "chat-recap.ron unreadable, starting empty");
            RecapFile::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RecapFile::default(),
        Err(e) => {
            crate::log_handled!(error, e, path = %path.display(), "chat-recap.ron read failed, starting empty");
            RecapFile::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::chat_history::ChatHistorySource;
    use crate::ai::recap_batcher::coarsen_if_needed;
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
            timestamp: Timestamp::now(),
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
        // Aurora's window is {grok(4), v3(5)}, Grok's is {v2(3), v3(5)}.
        assert_eq!(eligibility_cut_seq(&buf, 2, &[]), Some(3));
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
            timestamp: Timestamp::now(),
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
        let ts = Timestamp::now() - SignedDuration::from_hours(1);
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
        old.timestamp = ts - SignedDuration::from_mins(1);
        let mut newer = e(2, "y", None);
        newer.timestamp = ts + SignedDuration::from_mins(1);
        assert_eq!(rebind_through_seq(&log, &[old, newer]), 1);
    }

    #[test]
    fn trim_note_body_caps_bytes_on_char_boundary() {
        // The one-byte prefix makes the 768-byte cut land mid-char for the
        // multibyte units, so the cap must round down to a boundary.
        for unit in ["x", "ä", "🦀"] {
            let body = trim_note_body(&format!("a{}", unit.repeat(900)));
            assert!(
                body.len() <= RECAP_NOTE_BYTES,
                "{unit}: {} bytes",
                body.len()
            );
            // Fills the budget up to the last whole char that fits.
            assert!(body.len() > RECAP_NOTE_BYTES - unit.len(), "{unit}");
            let rest = body.strip_prefix('a').expect("prefix kept");
            assert!(rest.chars().all(|c| unit.starts_with(c)), "{unit}");
        }
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
    fn strip_quotes_keeps_text_after_unmatched_opener() {
        assert_eq!(strip_quotes("bob sagte „oops"), "bob sagte oops");
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
    async fn sheet_logins_prefers_newest_notes_when_capped() {
        let dir = tempfile::tempdir().unwrap();
        let handle = RecapHandle::load(dir.path().to_path_buf()).await;
        {
            let mut state = handle.inner.lock().await;
            state.file.primary.notes = vec![
                RecapNote {
                    body: "outside window".into(),
                    identities: vec![],
                    through_seq: 1,
                    generation: 0,
                },
                RecapNote {
                    body: "older".into(),
                    identities: (0..RECAP_SHEET_LOGINS)
                        .map(|i| RecapIdentity {
                            login: format!("older{i}"),
                            user_id: None,
                        })
                        .collect(),
                    through_seq: 2,
                    generation: 0,
                },
                RecapNote {
                    body: "newest".into(),
                    identities: vec![RecapIdentity {
                        login: "newest".into(),
                        user_id: None,
                    }],
                    through_seq: 3,
                    generation: 0,
                },
            ];
        }
        let logins = handle.sheet_logins().await;
        assert_eq!(logins.len(), RECAP_SHEET_LOGINS);
        assert!(logins.contains("newest"));
        assert!(!logins.contains("older15"));
    }

    fn batch_entry(username: &str, display_name: &str, agent: Agent) -> ChatHistoryEntry {
        ChatHistoryEntry {
            seq: 1,
            username: username.into(),
            display_name: Some(display_name.into()),
            user_id: None,
            text: "sigma".into(),
            source: ChatHistorySource::Bot,
            agent: Some(agent),
            timestamp: Timestamp::now(),
        }
    }

    #[test]
    fn format_batch_line_names_agent_once_without_self_tag() {
        for (login, persona, agent) in [
            ("grok", "Grok", Agent::Grok),
            ("aurora", "Aurora", Agent::Aurora),
        ] {
            let line = format_batch_line(&batch_entry(login, persona, agent), &[]);
            assert!(line.contains(&format!("] {persona}:")), "got: {line}");
            assert!(
                !line.contains(&format!("{persona} {persona}")),
                "got: {line}"
            );
            assert!(!line.contains("(self)"), "got: {line}");
        }
    }

    #[tokio::test]
    async fn coarsen_drops_at_max_generation() {
        let mut log = RecapChannelLog {
            notes: (0..4)
                .map(|i| RecapNote {
                    body: format!("n{i}"),
                    identities: vec![],
                    through_seq: i,
                    generation: RECAP_MAX_GENERATION,
                })
                .collect(),
            ..Default::default()
        };
        coarsen_if_needed(&mut log, 3, 3, None).await.unwrap();
        assert_eq!(log.notes.len(), 3);
        assert_eq!(log.notes[0].body, "n1");
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
                through_ts: Some(Timestamp::now()),
                through_fp: Some(LineFingerprint {
                    timestamp: Timestamp::now(),
                    username: "alice".into(),
                    text: "hi".into(),
                }),
            },
        };
        atomic_save_ron_async(&file, &path).await.unwrap();
        let back = load_recap_file(&path).await;
        assert_eq!(back.primary.notes[0].body, "raid vorbei");
    }

    #[tokio::test]
    async fn legacy_file_with_ai_channel_log_keeps_primary_notes() {
        let dir = tempfile::tempdir().unwrap();
        let path = recap_path(dir.path());
        crate::util::persist::atomic_write_bytes_async(
            br#"(
                primary: (notes: [(body: "raid vorbei", identities: [], through_seq: 9, generation: 0)], through_ts: None, through_fp: None),
                ai_channel: (notes: [(body: "alt", identities: [], through_seq: 1, generation: 0)], through_ts: None, through_fp: None),
            )"#,
            &path,
            "chat-recap.ron",
        )
        .await
        .unwrap();
        let back = load_recap_file(&path).await;
        assert_eq!(back.primary.notes[0].body, "raid vorbei");
    }
}
