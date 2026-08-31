//! Persisted seen-event ids, so a restart does not replay the feed window.
//!
//! ponytail: linear `contains` over a `VecDeque` capped at 500 — a set plus
//! a queue would be two structures to keep in sync for a scan that costs
//! nothing at this size. Add the `HashSet` if the cap ever grows a digit.

use std::collections::VecDeque;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub const FILENAME: &str = "alerts.ron";

/// Ids retained per feed. Comfortably longer than any plausible catch-up
/// batch, short enough that the linear scan stays free.
pub const MAX_SEEN: usize = 500;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenState {
    #[serde(default)]
    pub quake: VecDeque<String>,
    #[serde(default)]
    pub solar: VecDeque<String>,
}

impl SeenState {
    /// Load from `$DATA_DIR/alerts.ron`. A missing or unreadable file starts
    /// empty: losing the dedupe set costs at most one repeated announcement,
    /// which is not worth refusing to start over.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(FILENAME);
        if !path.exists() {
            info!("No alerts.ron found, starting with empty alert state");
            return Self::default();
        }
        match std::fs::read_to_string(&path).map(|d| ron::from_str::<Self>(&d)) {
            Ok(Ok(state)) => {
                info!(
                    quake = state.quake.len(),
                    solar = state.solar.len(),
                    "Loaded alert state"
                );
                state
            }
            Ok(Err(error)) => {
                warn!(?error, "alerts.ron did not parse, starting empty");
                Self::default()
            }
            Err(error) => {
                warn!(?error, "alerts.ron could not be read, starting empty");
                Self::default()
            }
        }
    }

    /// Persist atomically. A failed write is logged, never propagated — the
    /// alert already went to chat and the next poll rewrites the file.
    pub async fn save(&self, data_dir: &Path) {
        let path = data_dir.join(FILENAME);
        if let Err(error) = crate::util::persist::atomic_save_ron_async(self, &path).await {
            warn!(?error, "Failed to save alerts.ron");
        }
    }
}

/// Record `id` as seen, evicting the oldest id once the cap is reached.
pub fn mark(feed: &mut VecDeque<String>, id: String) {
    if feed.contains(&id) {
        return;
    }
    feed.push_back(id);
    while feed.len() > MAX_SEEN {
        feed.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = SeenState::default();
        mark(&mut s.quake, "a".into());
        mark(&mut s.solar, "b".into());
        s.save(dir.path()).await;

        let loaded = SeenState::load(dir.path());
        assert_eq!(loaded, s);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = SeenState::load(dir.path());
        assert!(s.quake.is_empty());
        assert!(s.solar.is_empty());
    }

    #[test]
    fn corrupt_file_loads_empty_rather_than_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILENAME), "definitely not ron").expect("write");
        let s = SeenState::load(dir.path());
        assert!(s.quake.is_empty());
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let mut feed = VecDeque::new();
        for i in 0..(MAX_SEEN + 10) {
            mark(&mut feed, i.to_string());
        }
        assert_eq!(feed.len(), MAX_SEEN);
        assert_eq!(feed.front().map(String::as_str), Some("10"));
        assert!(!feed.contains(&"9".to_string()));
    }

    #[test]
    fn mark_does_not_duplicate() {
        let mut feed = VecDeque::new();
        mark(&mut feed, "a".into());
        mark(&mut feed, "a".into());
        assert_eq!(feed.len(), 1);
    }
}
