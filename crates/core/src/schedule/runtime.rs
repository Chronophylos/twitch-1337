//! Per-schedule runtime state: last_fired_at, fires_today, day_anchor.
//!
//! Lives outside `settings.ron`: runtime state is state, not config. Stored
//! at `$DATA_DIR/schedule_runtime.ron`. Writes are debounced (flush at
//! most every 5s) and atomic (tmp + rename).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use jiff::{Timestamp, civil::Date};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

use super::Schedule;

use crate::util::{persist::atomic_save_ron_async, to_berlin};

const FILE_NAME: &str = "schedule_runtime.ron";
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleRuntime {
    #[serde(default)]
    pub last_fired_at: Option<Timestamp>,
    #[serde(default)]
    pub fires_today: u32,
    #[serde(default)]
    pub day_anchor: Option<Date>,
}

impl ScheduleRuntime {
    /// Roll over `fires_today` to 0 if `today` differs from `day_anchor`.
    /// Updates `day_anchor` in place. Returns `true` if a rollover happened.
    fn maybe_rollover(&mut self, today: Date) -> bool {
        match self.day_anchor {
            Some(d) if d == today => false,
            _ => {
                self.day_anchor = Some(today);
                self.fires_today = 0;
                true
            }
        }
    }
}

pub struct ScheduleRuntimeStore {
    path: PathBuf,
    data: Mutex<RuntimeState>,
}

struct RuntimeState {
    map: HashMap<String, ScheduleRuntime>,
    dirty: bool,
    last_flush: std::time::Instant,
}

impl ScheduleRuntimeStore {
    pub fn open(data_dir: &std::path::Path) -> Arc<Self> {
        let path = data_dir.join(FILE_NAME);
        let map = match std::fs::read_to_string(&path) {
            Ok(s) => match ron::from_str::<HashMap<String, ScheduleRuntime>>(&s) {
                Ok(map) => map,
                Err(error) => {
                    crate::log_handled!(
                        warn,
                        error,
                        "schedule runtime file unparseable; starting empty"
                    );
                    HashMap::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                info!("No schedule_runtime.ron found, starting empty");
                HashMap::new()
            }
            Err(error) => {
                crate::log_handled!(
                    warn,
                    error,
                    "schedule runtime file unreadable; starting empty"
                );
                HashMap::new()
            }
        };
        Arc::new(Self {
            path,
            data: Mutex::new(RuntimeState {
                map,
                dirty: false,
                last_flush: std::time::Instant::now(),
            }),
        })
    }

    /// Snapshot the current map (used by dashboard reads). `fires_today` is
    /// only rolled over by the next fire, so the copy is rolled over to `now`.
    pub async fn snapshot(&self, now: Timestamp) -> HashMap<String, ScheduleRuntime> {
        let today = to_berlin(now).date();
        let mut map = self.data.lock().await.map.clone();
        for runtime in map.values_mut() {
            runtime.maybe_rollover(today);
        }
        map
    }

    /// Record a fire for `name` at `at`. Rolls fires_today on day change.
    pub async fn record_fire(&self, name: &str, at: Timestamp) {
        let today = to_berlin(at).date();
        let mut g = self.data.lock().await;
        let entry = g.map.entry(name.to_owned()).or_default();
        entry.maybe_rollover(today);
        entry.last_fired_at = Some(at);
        entry.fires_today = entry.fires_today.saturating_add(1);
        g.dirty = true;
        if g.last_flush.elapsed() >= FLUSH_INTERVAL {
            Self::write(&self.path, &mut g, "schedule runtime flush failed").await;
        }
    }

    /// Drop counters of schedules that no longer exist, so deleted or renamed
    /// schedules do not accumulate in the file. Counters are keyed by name, so
    /// a renamed schedule starts from zero. Written with the next flush.
    pub async fn retain_schedules(&self, schedules: &[Schedule]) {
        let mut g = self.data.lock().await;
        let before = g.map.len();
        g.map
            .retain(|name, _| schedules.iter().any(|schedule| &schedule.name == name));
        if g.map.len() != before {
            g.dirty = true;
        }
    }

    /// Force-flush any pending writes. Call from graceful shutdown.
    pub async fn flush(&self) {
        let mut g = self.data.lock().await;
        if g.dirty {
            Self::write(&self.path, &mut g, "schedule runtime final flush failed").await;
        }
    }

    /// Writes while the caller holds the state lock, so a flush never races
    /// another on the shared `.tmp` path or lands an older snapshot over a
    /// newer one.
    // ponytail: dashboard snapshot reads wait on the disk write; it is a
    // small file at most every 5s.
    async fn write(path: &std::path::Path, g: &mut RuntimeState, what: &'static str) {
        match atomic_save_ron_async(&g.map, path).await {
            Ok(()) => {
                g.dirty = false;
                g.last_flush = std::time::Instant::now();
            }
            Err(error) => {
                crate::log_handled!(warn, error, failure = what, "schedule runtime write failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_fire_increments_counter() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = ScheduleRuntimeStore::open(dir.path());
        let now = Timestamp::now();
        s.record_fire("a", now).await;
        s.record_fire("a", now).await;
        let snap = s.snapshot(now).await;
        let r = snap.get("a").expect("present");
        assert_eq!(r.fires_today, 2);
        assert_eq!(r.last_fired_at, Some(now));
    }

    #[tokio::test]
    async fn rollover_resets_fires_today() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = ScheduleRuntimeStore::open(dir.path());
        let day1 = "2026-05-28T12:00:00Z".parse::<Timestamp>().unwrap();
        let day2 = "2026-05-29T12:00:00Z".parse::<Timestamp>().unwrap();
        s.record_fire("a", day1).await;
        s.record_fire("a", day1).await;
        s.record_fire("a", day2).await;
        let r = s.snapshot(day2).await.get("a").cloned().expect("present");
        assert_eq!(r.fires_today, 1);
        assert_eq!(r.day_anchor, Some(to_berlin(day2).date()));
    }

    #[tokio::test]
    async fn snapshot_resets_fires_today_at_berlin_midnight() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = ScheduleRuntimeStore::open(dir.path());
        // 23:30 and 00:30 Berlin: the same UTC day, different Berlin days.
        let before = "2026-05-28T21:30:00Z".parse::<Timestamp>().unwrap();
        let after = "2026-05-28T22:30:00Z".parse::<Timestamp>().unwrap();
        s.record_fire("a", before).await;

        let same_day = s.snapshot(before).await;
        assert_eq!(same_day["a"].fires_today, 1);

        let next_day = s.snapshot(after).await;
        assert_eq!(next_day["a"].fires_today, 0);
        assert_eq!(next_day["a"].last_fired_at, Some(before));
    }

    #[tokio::test]
    async fn reopen_restores_state() {
        let dir = tempfile::tempdir().expect("tmp");
        {
            let s = ScheduleRuntimeStore::open(dir.path());
            s.record_fire("a", Timestamp::now()).await;
            s.flush().await;
        }
        // The on-disk filename is the persistence contract across releases.
        assert!(dir.path().join("schedule_runtime.ron").exists());
        let s2 = ScheduleRuntimeStore::open(dir.path());
        assert!(s2.snapshot(Timestamp::now()).await.contains_key("a"));
    }

    #[tokio::test]
    async fn deleted_schedules_are_pruned_from_disk() {
        let dir = tempfile::tempdir().expect("tmp");
        let now = Timestamp::now();
        {
            let s = ScheduleRuntimeStore::open(dir.path());
            s.record_fire("kept", now).await;
            s.record_fire("deleted", now).await;
            let kept = Schedule {
                name: "kept".into(),
                message: "hi".into(),
                trigger: crate::schedule::Trigger::Calendar {
                    days: crate::schedule::WeekdaySet::default(),
                    at: jiff::civil::time(9, 0, 0, 0),
                },
                start_date: None,
                end_date: None,
                enabled: false,
                ping: None,
            };
            s.retain_schedules(&[kept]).await;
            s.flush().await;
        }
        let snap = ScheduleRuntimeStore::open(dir.path()).snapshot(now).await;
        assert!(
            snap.contains_key("kept"),
            "disabled schedules keep counters"
        );
        assert!(!snap.contains_key("deleted"));
    }
}
