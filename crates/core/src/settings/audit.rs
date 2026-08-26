//! Append-only JSON-lines audit log for settings changes.

#[cfg(any(test, feature = "testing"))]
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{Schedule, Settings};

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts: DateTime<chrono_tz::Tz>,
    pub actor_id: String,
    pub actor_login: String,
    pub changes: Vec<AuditChange>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditChange {
    pub key: String,
    /// Old value, JSON-encoded. `null` if the field had no override (was at default).
    pub old: serde_json::Value,
    /// New value, JSON-encoded.
    pub new: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] serde_json::Error),
}

pub trait AuditLog: Send + Sync {
    fn append(&self, entry: &AuditEntry) -> Result<(), AuditError>;
}

pub struct FileAuditLog {
    path: std::path::PathBuf,
}

impl FileAuditLog {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Append any serializable value as one JSON line. Shared by the settings
    /// `AuditEntry` trait impl below and the web memory-audit entry, so both
    /// audit logs use the identical create+append+sync writer.
    pub fn append_serializable<S: serde::Serialize>(&self, entry: &S) -> Result<(), AuditError> {
        use std::io::Write as _;
        let line = serde_json::to_string(entry)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{line}")?;
        f.sync_all()?;
        Ok(())
    }
}

impl AuditLog for FileAuditLog {
    fn append(&self, entry: &AuditEntry) -> Result<(), AuditError> {
        self.append_serializable(entry)
    }
}

#[cfg(any(test, feature = "testing"))]
pub struct MemoryAuditLog {
    entries: Mutex<Vec<AuditEntry>>,
}

#[cfg(any(test, feature = "testing"))]
impl MemoryAuditLog {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> Vec<AuditEntry> {
        self.entries.lock().unwrap().clone()
    }
}

#[cfg(any(test, feature = "testing"))]
impl Default for MemoryAuditLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "testing"))]
impl AuditLog for MemoryAuditLog {
    fn append(&self, entry: &AuditEntry) -> Result<(), AuditError> {
        self.entries.lock().unwrap().push(entry.clone());
        Ok(())
    }
}

pub fn berlin_now(now_utc: DateTime<Utc>) -> DateTime<chrono_tz::Tz> {
    now_utc.with_timezone(&chrono_tz::Europe::Berlin)
}

/// Produce the per-field `AuditChange` list between two resolved snapshots.
/// Diffs via a generic JSON tree walk (`tree::diff`); `schedules` are handled
/// by name across the vec instead of as a wholesale array.
pub(super) fn diff_changes(prior: &Settings, next: &Settings) -> Vec<AuditChange> {
    let mut p = serde_json::to_value(prior).expect("serialize prior settings");
    let mut n = serde_json::to_value(next).expect("serialize next settings");
    if let Some(o) = p.as_object_mut() {
        o.remove("schedules");
        o.remove("schema_version");
    }
    if let Some(o) = n.as_object_mut() {
        o.remove("schedules");
        o.remove("schema_version");
    }
    let mut out: Vec<AuditChange> = super::tree::diff(&p, &n)
        .into_iter()
        .map(|(key, old, new)| AuditChange { key, old, new })
        .collect();

    // Schedules — diff by name across vec
    {
        use std::collections::{BTreeMap, BTreeSet};
        let prior_map: BTreeMap<&str, &Schedule> = prior
            .schedules
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        let next_map: BTreeMap<&str, &Schedule> = next
            .schedules
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        let all_names: BTreeSet<&str> = prior_map
            .keys()
            .copied()
            .chain(next_map.keys().copied())
            .collect();
        for name in all_names {
            let p = prior_map.get(name).copied();
            let n = next_map.get(name).copied();
            if p != n {
                out.push(AuditChange {
                    key: format!("schedules.{name}"),
                    old: p
                        .map(|s| serde_json::to_value(s).expect("serialize prior schedule"))
                        .unwrap_or(serde_json::Value::Null),
                    new: n
                        .map(|s| serde_json::to_value(s).expect("serialize next schedule"))
                        .unwrap_or(serde_json::Value::Null),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> AuditEntry {
        AuditEntry {
            ts: berlin_now("2026-05-12T13:37:00Z".parse::<DateTime<Utc>>().unwrap()),
            actor_id: "12345678".into(),
            actor_login: "chronophylos".into(),
            changes: vec![AuditChange {
                key: "cooldowns.ai".into(),
                old: serde_json::Value::Number(30.into()),
                new: serde_json::Value::Number(15.into()),
            }],
        }
    }

    #[test]
    fn memory_log_records_entries() {
        let log = MemoryAuditLog::new();
        let e = sample_entry();
        log.append(&e).expect("append");
        log.append(&e).expect("append twice");
        assert_eq!(log.snapshot().len(), 2);
    }

    #[test]
    fn file_log_appends_one_json_line_per_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.log");
        let log = FileAuditLog::new(&path);
        let e = sample_entry();
        log.append(&e).expect("first");
        log.append(&e).expect("second");
        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let parsed: serde_json::Value = serde_json::from_str(line).expect("valid json");
            assert_eq!(parsed["actor_id"], "12345678");
            assert_eq!(parsed["changes"][0]["key"], "cooldowns.ai");
        }
    }

    #[test]
    fn file_log_survives_truncation_between_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.log");
        let log = FileAuditLog::new(&path);
        log.append(&sample_entry()).expect("first");
        std::fs::remove_file(&path).expect("remove");
        log.append(&sample_entry()).expect("second after unlink");
        assert!(path.exists());
        let lines = std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .count();
        assert_eq!(lines, 1);
    }

    fn make_schedule(name: &str, message: &str) -> Schedule {
        use crate::schedule::{Trigger, WeekdaySet};
        Schedule {
            name: name.into(),
            message: message.into(),
            trigger: Trigger::Interval {
                every: std::time::Duration::from_secs(3600),
                days: WeekdaySet::default(),
                active_from: None,
                active_to: None,
            },
            start_date: None,
            end_date: None,
            enabled: true,
        }
    }

    #[test]
    fn diff_emits_service_tier_changes() {
        let prior = Settings::compiled_defaults();
        let mut next = Settings::compiled_defaults();
        next.ai.profiles[0].service_tier = Some("flex".to_string());
        let changes = diff_changes(&prior, &next);
        let keys: Vec<&str> = changes.iter().map(|c| c.key.as_str()).collect();
        // Profiles diff as a whole list (arrays are not recursed into).
        assert!(keys.contains(&"ai.profiles"), "got {keys:?}");
    }

    #[test]
    fn diff_emits_emotes_pinned_change() {
        use crate::settings::ai::AiEmotes;
        let mut prior = Settings::compiled_defaults();
        let mut next = Settings::compiled_defaults();
        prior.ai.emotes = Some(AiEmotes::default());
        next.ai.emotes = Some(AiEmotes {
            pinned_emotes: vec!["PepeLa".into(), "okjj".into(), "Clueless".into()],
            ..AiEmotes::default()
        });
        let changes = diff_changes(&prior, &next);
        let keys: Vec<&str> = changes.iter().map(|c| c.key.as_str()).collect();
        assert!(keys.contains(&"ai.emotes.pinned_emotes"), "got {keys:?}");
    }

    #[test]
    fn diff_emits_twitch_section_changes() {
        let prior = Settings::compiled_defaults();
        let mut next = Settings::compiled_defaults();
        next.twitch.expected_latency = 200;
        next.twitch.hidden_admins = vec!["111".into()];
        next.twitch.admin_channel = Some("admins".into());
        let changes = diff_changes(&prior, &next);
        let keys: Vec<&str> = changes.iter().map(|c| c.key.as_str()).collect();
        assert!(
            keys.iter().any(|k| k.starts_with("twitch.")),
            "diff must include at least one twitch.* entry; got {keys:?}"
        );
        assert!(keys.contains(&"twitch.expected_latency"), "got {keys:?}");
        assert!(keys.contains(&"twitch.hidden_admins"), "got {keys:?}");
        assert!(keys.contains(&"twitch.admin_channel"), "got {keys:?}");
    }

    #[test]
    fn diff_emits_schedule_added_removed_modified() {
        let mut prior = Settings::compiled_defaults();
        let mut next = Settings::compiled_defaults();
        prior.schedules = vec![make_schedule("keep", "old"), make_schedule("drop", "x")];
        next.schedules = vec![
            make_schedule("keep", "new"), // modified
            make_schedule("add", "y"),
        ];
        let changes = diff_changes(&prior, &next);
        let keys: Vec<&str> = changes.iter().map(|c| c.key.as_str()).collect();
        assert!(keys.contains(&"schedules.keep"), "got {keys:?}");
        assert!(keys.contains(&"schedules.drop"), "got {keys:?}");
        assert!(keys.contains(&"schedules.add"), "got {keys:?}");
    }

    #[test]
    fn append_serializable_writes_one_json_line_per_call() {
        #[derive(serde::Serialize)]
        struct Tiny {
            a: u32,
            b: &'static str,
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("memory_audit.log");
        let log = FileAuditLog::new(&path);
        log.append_serializable(&Tiny { a: 1, b: "hi" })
            .expect("first");
        log.append_serializable(&Tiny { a: 2, b: "yo" })
            .expect("second");
        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).expect("valid json");
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], "hi");
        // Assert the second line too, so the test proves the writer *appends*
        // (distinct values per call) rather than overwriting.
        let v2: serde_json::Value = serde_json::from_str(lines[1]).expect("valid json");
        assert_eq!(v2["a"], 2);
        assert_eq!(v2["b"], "yo");
    }
}
