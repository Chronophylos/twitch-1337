use std::collections::{HashMap, HashSet};

use jiff::civil::{Date, Time};
use jiff::{SignedDuration, Timestamp, ZonedDateTime};

use crate::emote_tracker::{EmoteMeaningRow, EmoteRow, GlobalCatalogEmote};

pub const UNDERUSED_MIN_LINES: i64 = 3;
pub const REGENERATE_AFTER_MS: i64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteWork {
    pub emote_id: String,
    pub name: String,
}

/// Select the capped nightly batch in the six ADR-0014 priority tiers. Every
/// tier is deduped by stable 7TV id and fully curated rows never enter it.
pub(crate) fn select_batch(
    active: &[EmoteRow],
    meanings: &[EmoteMeaningRow],
    post_window_counts: &HashMap<String, i64>,
    globals: &[GlobalCatalogEmote],
    now_ms: i64,
    min_usages: u32,
    batch_size: usize,
) -> Vec<EmoteWork> {
    let meanings: HashMap<&str, &EmoteMeaningRow> = meanings
        .iter()
        .map(|row| (row.emote_id.as_str(), row))
        .collect();
    let active_by_id: HashMap<&str, &EmoteRow> = active
        .iter()
        .map(|row| (row.emote_id.as_str(), row))
        .collect();
    let count = |id: &str| post_window_counts.get(id).copied().unwrap_or(0);
    let min = i64::from(min_usages);
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |id: &str, name: &str| {
        if seen.insert(id.to_owned()) {
            selected.push(EmoteWork {
                emote_id: id.to_owned(),
                name: name.to_owned(),
            });
        }
    };
    let automatable = |row: &EmoteMeaningRow| !row.curated.is_all();

    // 1. Active channel emotes with no row.
    for emote in active
        .iter()
        .filter(|e| !meanings.contains_key(e.emote_id.as_str()))
    {
        push(&emote.emote_id, &emote.name);
    }

    // 2. Rows generated without context that now have enough post-window lines.
    let mut tier: Vec<&EmoteMeaningRow> = active
        .iter()
        .filter_map(|emote| meanings.get(emote.emote_id.as_str()).copied())
        .filter(|row| automatable(row) && row.context_lines == 0 && count(&row.emote_id) >= min)
        .collect();
    tier.sort_by(|a, b| a.emote_id.cmp(&b.emote_id));
    for row in tier {
        let name = active_by_id
            .get(row.emote_id.as_str())
            .map(|emote| emote.name.as_str())
            .unwrap_or_else(|| row.name.as_deref().unwrap_or(""));
        push(&row.emote_id, name);
    }

    // 3. Renames keep their row but jump the queue.
    for emote in active {
        if let Some(row) = meanings.get(emote.emote_id.as_str()).copied()
            && automatable(row)
            && row.name.as_deref() != Some(emote.name.as_str())
        {
            push(&emote.emote_id, &emote.name);
        }
    }

    // 4. Underused fallback: at least three lines and a 30-day cadence.
    let mut tier: Vec<&EmoteMeaningRow> = active
        .iter()
        .filter_map(|emote| meanings.get(emote.emote_id.as_str()).copied())
        .filter(|row| {
            let uses = count(&row.emote_id);
            automatable(row)
                && uses >= UNDERUSED_MIN_LINES
                && uses < min
                && row
                    .generated_at
                    .is_some_and(|at| now_ms.saturating_sub(at) >= REGENERATE_AFTER_MS)
        })
        .collect();
    tier.sort_by(|a, b| {
        a.generated_at
            .cmp(&b.generated_at)
            .then_with(|| a.emote_id.cmp(&b.emote_id))
    });
    for row in tier {
        if let Some(emote) = active_by_id.get(row.emote_id.as_str()) {
            push(&row.emote_id, &emote.name);
        }
    }

    // 5. Oldest generated rows among channel emotes meeting the normal usage
    // floor in the last 30 days. Below-minusages rows are tier 4 only, so the
    // <3-line case is never regenerated through this backdoor.
    let mut tier: Vec<&EmoteMeaningRow> = active
        .iter()
        .filter_map(|emote| meanings.get(emote.emote_id.as_str()).copied())
        .filter(|row| automatable(row) && row.generated_at.is_some() && count(&row.emote_id) >= min)
        .collect();
    tier.sort_by(|a, b| {
        a.generated_at
            .cmp(&b.generated_at)
            .then_with(|| a.emote_id.cmp(&b.emote_id))
    });
    for row in tier {
        if let Some(emote) = active_by_id.get(row.emote_id.as_str()) {
            push(&row.emote_id, &emote.name);
        }
    }

    // 6. Used globals only, after every channel-emote tier. IDs that are also
    // active locally were already handled above and are never double-counted.
    let mut tier: Vec<(&GlobalCatalogEmote, Option<&EmoteMeaningRow>)> = globals
        .iter()
        .filter(|global| {
            !active_by_id.contains_key(global.id.as_str())
                && count(&global.id) >= min
                && meanings
                    .get(global.id.as_str())
                    .is_none_or(|row| automatable(row))
        })
        .map(|global| (global, meanings.get(global.id.as_str()).copied()))
        .collect();
    tier.sort_by(|(a, a_row), (b, b_row)| match (a_row, b_row) {
        (None, None) => a.id.cmp(&b.id),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a_row), Some(b_row)) => a_row
            .generated_at
            .cmp(&b_row.generated_at)
            .then_with(|| a.id.cmp(&b.id)),
    });
    for (global, _) in tier {
        push(&global.id, &global.name);
    }

    selected.truncate(batch_size);
    selected
}

/// Resolve the next Berlin wallclock run, using the Dreamer's DST-gap/fold
/// behavior. `None` only at jiff's calendar bounds.
pub fn next_run_at(now: ZonedDateTime, run_at: Time) -> Option<Timestamp> {
    if let Some(target) = resolve_run_at(now.date(), run_at)
        && target > now
    {
        return Some(target.timestamp());
    }
    resolve_run_at(now.date().tomorrow().ok()?, run_at).map(|next| next.timestamp())
}

pub(crate) fn resolve_run_at(date: Date, time: Time) -> Option<ZonedDateTime> {
    let local = date.to_datetime(time);
    if let Ok(resolved) = crate::util::BERLIN
        .to_ambiguous_zoned_datetime(local)
        .unambiguous()
    {
        return Some(resolved);
    }
    crate::util::BERLIN
        .to_ambiguous_zoned_datetime(local + SignedDuration::from_hours(1))
        .unambiguous()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::CuratedFields;

    fn emote(id: &str, name: &str) -> EmoteRow {
        EmoteRow {
            emote_id: id.into(),
            name: name.into(),
            active: true,
            updated_at: 1,
        }
    }

    fn meaning(id: &str, name: &str, generated_at: Option<i64>, context: i64) -> EmoteMeaningRow {
        EmoteMeaningRow {
            emote_id: id.into(),
            appearance: "auto".into(),
            meaning: None,
            usage: None,
            avoid: None,
            updated_at: generated_at.unwrap_or(0),
            name: Some(name.into()),
            curated: CuratedFields::default(),
            generated_at,
            context_lines: context,
            usage_count: 0,
        }
    }

    #[test]
    fn priority_cap_underused_and_used_globals() {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        let now = 100 * DAY;
        let active = vec![
            emote("new", "New"),
            emote("context", "Context"),
            emote("renamed", "NewName"),
            emote("underused", "Underused"),
            emote("old", "Old"),
            emote("too-rare", "TooRare"),
        ];
        let meanings = vec![
            meaning("context", "Context", Some(90 * DAY), 0),
            meaning("renamed", "OldName", Some(90 * DAY), 10),
            meaning("underused", "Underused", Some(60 * DAY), 3),
            meaning("old", "Old", Some(50 * DAY), 5),
            meaning("too-rare", "TooRare", Some(50 * DAY), 2),
        ];
        let counts = HashMap::from([
            ("context".into(), 10),
            ("renamed".into(), 10),
            ("underused".into(), 3),
            ("old".into(), 10),
            ("too-rare".into(), 2),
            ("used-global".into(), 10),
            ("unused-global".into(), 0),
        ]);
        let globals = vec![
            GlobalCatalogEmote {
                id: "used-global".into(),
                name: "UsedGlobal".into(),
            },
            GlobalCatalogEmote {
                id: "unused-global".into(),
                name: "UnusedGlobal".into(),
            },
        ];
        let selected = select_batch(&active, &meanings, &counts, &globals, now, 10, 50);
        assert_eq!(
            selected
                .iter()
                .map(|w| w.emote_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "new",
                "context",
                "renamed",
                "underused",
                "old",
                "used-global"
            ]
        );
        assert!(!selected.iter().any(|w| w.emote_id == "too-rare"));
        assert!(!selected.iter().any(|w| w.emote_id == "unused-global"));

        let capped = select_batch(&active, &meanings, &counts, &globals, now, 10, 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].emote_id, "new");
        assert_eq!(capped[1].emote_id, "context");
    }

    #[test]
    fn missing_globals_precede_older_global_rows_before_the_cap() {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        let globals = ["a-old", "b-new", "y-missing", "z-missing"].map(|id| GlobalCatalogEmote {
            id: id.into(),
            name: id.into(),
        });
        let meanings = vec![
            meaning("a-old", "a-old", Some(10 * DAY), 1),
            meaning("b-new", "b-new", Some(20 * DAY), 1),
        ];
        let counts = globals
            .iter()
            .map(|global| (global.id.clone(), 10))
            .collect();
        let selected = select_batch(&[], &meanings, &counts, &globals, 100 * DAY, 10, 3);
        assert_eq!(
            selected
                .iter()
                .map(|work| work.emote_id.as_str())
                .collect::<Vec<_>>(),
            vec!["y-missing", "z-missing", "a-old"]
        );
    }

    #[test]
    fn fully_curated_rows_are_never_selected() {
        let active = vec![emote("e", "E")];
        let mut row = meaning("e", "old", None, 0);
        for field in crate::emote_tracker::MeaningField::ALL {
            row.curated = mark_curated(row.curated, field);
        }
        let selected = select_batch(
            &active,
            &[row],
            &HashMap::from([("e".into(), 100)]),
            &[],
            100 * REGENERATE_AFTER_MS,
            10,
            50,
        );
        assert!(selected.is_empty());
    }

    fn mark_curated(
        fields: CuratedFields,
        field: crate::emote_tracker::MeaningField,
    ) -> CuratedFields {
        CuratedFields::from_bits(fields.bits() | field.bit())
    }
}
