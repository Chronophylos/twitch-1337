use chrono::{DateTime, Utc};

use crate::memory::Memory;
use crate::memory::store::MemoryStore;

/// Returns a list of `(key, reason)` tuples for memories flagged by the
/// deterministic pre-filter: confidence below 10 and last access older than
/// 60 days. Reported by the consolidation pass to the caller before any
/// LLM-driven curation runs.
pub fn hard_drop_candidates(store: &MemoryStore, now: DateTime<Utc>) -> Vec<(String, String)> {
    store
        .memories
        .iter()
        .filter_map(|(k, m)| {
            let stale_days = (now - m.last_accessed).num_days();
            if m.confidence < 10 && stale_days > 60 {
                Some((
                    k.clone(),
                    format!("confidence={} stale_days={}", m.confidence, stale_days),
                ))
            } else {
                None
            }
        })
        .collect()
}

/// Boost `confidence` by `+5 * (distinct_sources - 1)`, where `distinct_sources`
/// counts non-sentinel entries in `sources` (`"legacy"` and `"__identity__"`
/// are excluded). Clamps the result to `[0, 100]`.
pub fn corroboration_boost(m: &Memory) -> u8 {
    let distinct: usize = m
        .sources
        .iter()
        .filter(|s| s.as_str() != "legacy" && s.as_str() != "__identity__")
        .count();
    // usize → u32: sources are bounded per memory; saturating cast is safe.
    let bonus = u32::try_from(distinct.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .saturating_mul(5);
    u8::try_from(u32::from(m.confidence).saturating_add(bonus).min(100)).unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Scope;

    fn m_with(conf: u8, stale_days: i64, sources: Vec<String>) -> Memory {
        use chrono::Duration;
        let now = Utc::now();
        Memory {
            fact: "x".into(),
            scope: Scope::Lore,
            sources,
            confidence: conf,
            created_at: now,
            updated_at: now,
            last_accessed: now - Duration::days(stale_days),
            access_count: 0,
        }
    }

    #[test]
    fn hard_drop_catches_low_conf_stale() {
        let mut s = MemoryStore::default();
        s.memories
            .insert("a".into(), m_with(5, 90, vec!["alice".into()]));
        s.memories
            .insert("b".into(), m_with(50, 90, vec!["alice".into()]));
        s.memories
            .insert("c".into(), m_with(5, 30, vec!["alice".into()]));
        let drops = hard_drop_candidates(&s, Utc::now());
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].0, "a");
    }

    #[test]
    fn corroboration_boost_ignores_sentinels() {
        let m = m_with(
            70,
            0,
            vec!["alice".into(), "legacy".into(), "__identity__".into()],
        );
        // only alice counts; distinct=1 → bonus=0
        assert_eq!(corroboration_boost(&m), 70);
    }

    #[test]
    fn corroboration_boost_rewards_multiple_sources() {
        let m = m_with(70, 0, vec!["alice".into(), "bob".into(), "carol".into()]);
        // 3 distinct → +10
        assert_eq!(corroboration_boost(&m), 80);
    }

    #[test]
    fn corroboration_boost_clamps_at_100() {
        let m = m_with(
            95,
            0,
            vec![
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
                "f".into(),
            ],
        );
        assert_eq!(corroboration_boost(&m), 100);
    }
}
