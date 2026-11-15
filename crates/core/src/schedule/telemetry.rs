//! Per-schedule runtime telemetry: last_fired_at, fires_today, day_anchor.
//!
//! Lives outside `settings.ron`: telemetry is state, not config. Stored
//! at `$DATA_DIR/schedule_runtime.ron`. Writes are debounced (flush at
//! most every 5s) and atomic (tmp + rename).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Europe::Berlin;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::warn;

use crate::util::persist::atomic_save_ron_async;

const FILE_NAME: &str = "schedule_runtime.ron";
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleRuntime {
    #[serde(default)]
    pub last_fired_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub fires_today: u32,
    #[serde(default)]
    pub day_anchor: Option<NaiveDate>,
}

impl ScheduleRuntime {
    /// Roll over `fires_today` to 0 if `today` differs from `day_anchor`.
    /// Updates `day_anchor` in place. Returns `true` if a rollover happened.
    fn maybe_rollover(&mut self, today: NaiveDate) -> bool {
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

pub struct TelemetryStore {
    path: PathBuf,
    data: Mutex<TelemetryState>,
}

struct TelemetryState {
    map: HashMap<String, ScheduleRuntime>,
    dirty: bool,
    last_flush: std::time::Instant,
}

impl TelemetryStore {
    pub fn open(data_dir: &std::path::Path) -> Arc<Self> {
        let path = data_dir.join(FILE_NAME);
        let map = match std::fs::read_to_string(&path) {
            Ok(s) => ron::from_str::<HashMap<String, ScheduleRuntime>>(&s).unwrap_or_else(|e| {
                warn!(?e, "telemetry file unparseable; starting empty");
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        Arc::new(Self {
            path,
            data: Mutex::new(TelemetryState {
                map,
                dirty: false,
                last_flush: std::time::Instant::now(),
            }),
        })
    }

    /// Snapshot the current map (used by dashboard reads). `fires_today` is
    /// only rolled over by the next fire, so the copy is rolled over to `now`.
    pub async fn snapshot(&self, now: DateTime<Utc>) -> HashMap<String, ScheduleRuntime> {
        let today = now.with_timezone(&Berlin).date_naive();
        let mut map = self.data.lock().await.map.clone();
        for runtime in map.values_mut() {
            runtime.maybe_rollover(today);
        }
        map
    }

    /// Record a fire for `name` at `at`. Rolls fires_today on day change.
    pub async fn record_fire(&self, name: &str, at: DateTime<Utc>) {
        let today = at.with_timezone(&Berlin).date_naive();
        let mut g = self.data.lock().await;
        let entry = g.map.entry(name.to_owned()).or_default();
        entry.maybe_rollover(today);
        entry.last_fired_at = Some(at);
        entry.fires_today = entry.fires_today.saturating_add(1);
        g.dirty = true;
        if g.last_flush.elapsed() >= FLUSH_INTERVAL {
            Self::write(&self.path, &mut g, "telemetry flush failed").await;
        }
    }

    /// Force-flush any pending writes. Call from graceful shutdown.
    pub async fn flush(&self) {
        let mut g = self.data.lock().await;
        if g.dirty {
            Self::write(&self.path, &mut g, "telemetry final flush failed").await;
        }
    }

    /// Writes while the caller holds the state lock, so concurrent schedule
    /// children never race on the shared `.tmp` path or land an older
    /// snapshot over a newer one.
    // ponytail: dashboard snapshot reads wait on the disk write; it is a
    // small file at most every 5s.
    async fn write(path: &std::path::Path, g: &mut TelemetryState, what: &'static str) {
        match atomic_save_ron_async(&g.map, path).await {
            Ok(()) => {
                g.dirty = false;
                g.last_flush = std::time::Instant::now();
            }
            Err(e) => warn!(?e, "{what}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[tokio::test]
    async fn record_fire_increments_counter() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = TelemetryStore::open(dir.path());
        let now = Utc::now();
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
        let s = TelemetryStore::open(dir.path());
        let day1 = Utc
            .with_ymd_and_hms(2026, 5, 28, 12, 0, 0)
            .single()
            .unwrap();
        let day2 = Utc
            .with_ymd_and_hms(2026, 5, 29, 12, 0, 0)
            .single()
            .unwrap();
        s.record_fire("a", day1).await;
        s.record_fire("a", day1).await;
        s.record_fire("a", day2).await;
        let r = s.snapshot(day2).await.get("a").cloned().expect("present");
        assert_eq!(r.fires_today, 1);
        assert_eq!(r.day_anchor, Some(day2.with_timezone(&Berlin).date_naive()));
    }

    #[tokio::test]
    async fn snapshot_resets_fires_today_at_berlin_midnight() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = TelemetryStore::open(dir.path());
        // 23:30 and 00:30 Berlin: the same UTC day, different Berlin days.
        let before = Utc
            .with_ymd_and_hms(2026, 5, 28, 21, 30, 0)
            .single()
            .unwrap();
        let after = Utc
            .with_ymd_and_hms(2026, 5, 28, 22, 30, 0)
            .single()
            .unwrap();
        s.record_fire("a", before).await;

        let same_day = s.snapshot(before).await;
        assert_eq!(same_day["a"].fires_today, 1);

        let next_day = s.snapshot(after).await;
        assert_eq!(next_day["a"].fires_today, 0);
        assert_eq!(next_day["a"].last_fired_at, Some(before));
    }

    #[tokio::test]
    async fn flush_writes_atomic_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = TelemetryStore::open(dir.path());
        s.record_fire("a", Utc::now()).await;
        s.flush().await;
        let path = dir.path().join("schedule_runtime.ron");
        assert!(path.exists());
        let parsed: HashMap<String, ScheduleRuntime> =
            ron::from_str(&std::fs::read_to_string(&path).expect("read")).expect("de");
        assert!(parsed.contains_key("a"));
    }

    #[tokio::test]
    async fn reopen_restores_state() {
        let dir = tempfile::tempdir().expect("tmp");
        {
            let s = TelemetryStore::open(dir.path());
            s.record_fire("a", Utc::now()).await;
            s.flush().await;
        }
        let s2 = TelemetryStore::open(dir.path());
        assert!(s2.snapshot(Utc::now()).await.contains_key("a"));
    }
}
