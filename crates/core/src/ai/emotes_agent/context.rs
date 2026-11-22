//! Join sampled usage rows back to their verbatim daily Transcript lines.
//! Usage remains the index: message text is never copied into SQLite.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr as _};
use jiff::civil::{Date, Time};
use jiff::{Timestamp, ZonedDateTime};

use crate::emote_tracker::UsageSample;
use crate::util::to_berlin;

#[derive(Clone)]
pub struct ContextSampler {
    transcripts_dir: PathBuf,
}

impl ContextSampler {
    pub fn new(transcripts_dir: PathBuf) -> Self {
        Self { transcripts_dir }
    }

    /// Resolve newest-first samples to at most the same number of distinct,
    /// emote-bearing Transcript lines. Dreamer's Berlin `run_at` determines
    /// whether a line owns a dated rotation file or the open `today.md`.
    /// Missing/rotated-away lines are skipped; an unreadable existing file is
    /// an operational error rather than silently invented context.
    pub async fn transcript_lines(
        &self,
        samples: &[UsageSample],
        emote_name: &str,
        now: Timestamp,
        run_at: Option<Time>,
    ) -> Result<Vec<String>> {
        let now_local = to_berlin(now);
        let current_window = run_at
            .map(|time| rotation_window_start(&now_local, time))
            .transpose()?;
        let mut files: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
        let mut consumed: HashMap<PathBuf, HashSet<usize>> = HashMap::new();
        let mut lines = Vec::with_capacity(samples.len());
        for sample in samples {
            let timestamp = Timestamp::from_millisecond(sample.at)
                .wrap_err("usage sample timestamp outside jiff's range")?;
            let local = to_berlin(timestamp);
            let path = match (current_window, run_at) {
                (Some(current), Some(time)) => {
                    let window = rotation_window_start(&local, time)?;
                    if window == current {
                        self.transcripts_dir.join("today.md")
                    } else {
                        self.transcripts_dir
                            .join(format!("{}.md", window.strftime("%Y-%m-%d")))
                    }
                }
                _ => self.transcripts_dir.join("today.md"),
            };
            if !files.contains_key(&path) {
                let loaded = read_lines(&path).await?;
                files.insert(path.clone(), loaded);
            }
            let Some(day_lines) = files.get(&path).and_then(Option::as_deref) else {
                continue;
            };
            let clock = local.strftime("%H:%M:%S").to_string();
            let mut found = None;
            for (index, line) in day_lines.iter().enumerate().rev() {
                if consumed
                    .get(&path)
                    .is_some_and(|indices| indices.contains(&index))
                {
                    continue;
                }
                if transcript_line_matches(line, &clock, &sample.twitch_user_id, emote_name) {
                    found = Some((index, line.clone()));
                    break;
                }
            }
            if let Some((index, line)) = found {
                consumed.entry(path).or_default().insert(index);
                lines.push(line);
            }
        }
        Ok(lines)
    }
}

/// Return the Berlin civil date whose `run_at` opens the rotation window
/// containing `local`. A line before today's rotation belongs to yesterday's
/// window even when its civil date is already today.
fn rotation_window_start(local: &ZonedDateTime, run_at: Time) -> Result<Date> {
    let today_rotation = super::scheduler::resolve_run_at(local.date(), run_at)
        .ok_or_else(|| eyre::eyre!("transcript rotation time outside jiff's range"))?;
    if today_rotation.timestamp() <= local.timestamp() {
        Ok(local.date())
    } else {
        Ok(local.date().yesterday()?)
    }
}

async fn read_lines(path: &Path) -> Result<Option<Vec<String>>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text.lines().map(str::to_owned).collect())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).wrap_err_with(|| format!("read transcript {}", path.display())),
    }
}

fn transcript_line_matches(
    line: &str,
    clock: &str,
    twitch_user_id: &str,
    emote_name: &str,
) -> bool {
    if line.get(..8) != Some(clock) || line.get(8..10) != Some("  ") {
        return false;
    }
    let Some((actor, text)) = line.get(10..).and_then(|rest| rest.split_once(": ")) else {
        return false;
    };
    actor
        .rsplit_once('#')
        .is_some_and(|(_, id)| id == twitch_user_id)
        && text.split_whitespace().any(|token| token == emote_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::persist::atomic_write_bytes_async;

    fn timestamp(at: &str) -> Timestamp {
        at.parse().unwrap()
    }

    fn sample(at: &str, id: &str) -> UsageSample {
        UsageSample {
            at: timestamp(at).as_millisecond(),
            twitch_user_id: id.into(),
            login: Some("alice".into()),
        }
    }

    fn run_at(value: &str) -> Option<Time> {
        Some(Time::strptime("%H:%M", value).unwrap())
    }

    #[tokio::test]
    async fn joins_timestamp_and_user_id_to_verbatim_lines() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write_bytes_async(
            "01:00:01  alice#42: erst Kappa dann Kontext\n\
             01:00:01  other#43: Kappa falsche Person\n\
             01:00:03  alice#42: Kappa zweite Zeile\n"
                .as_bytes(),
            &dir.path().join("2026-01-01.md"),
            "transcript",
        )
        .await
        .unwrap();
        let sampler = ContextSampler::new(dir.path().to_path_buf());
        let lines = sampler
            .transcript_lines(
                &[
                    sample("2026-01-01T00:00:03Z", "42"),
                    sample("2026-01-01T00:00:01Z", "42"),
                    sample("2026-01-01T00:00:02Z", "42"),
                ],
                "Kappa",
                timestamp("2026-01-02T00:00:00Z"),
                run_at("00:00"),
            )
            .await
            .unwrap();
        assert_eq!(
            lines,
            vec![
                "01:00:03  alice#42: Kappa zweite Zeile",
                "01:00:01  alice#42: erst Kappa dann Kontext"
            ]
        );
    }

    #[tokio::test]
    async fn disabled_rotation_reads_today_for_any_usage_date() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write_bytes_async(
            b"01:00:01  alice#42: Kappa noch offen\n",
            &dir.path().join("today.md"),
            "transcript",
        )
        .await
        .unwrap();
        let lines = ContextSampler::new(dir.path().to_path_buf())
            .transcript_lines(
                &[sample("2026-01-01T00:00:01Z", "42")],
                "Kappa",
                timestamp("2026-01-02T00:00:00Z"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(lines, vec!["01:00:01  alice#42: Kappa noch offen"]);
    }

    #[tokio::test]
    async fn previous_dated_file_covers_post_midnight_rotation_window() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write_bytes_async(
            b"01:00:01  alice#42: Kappa nach Mitternacht\n",
            &dir.path().join("2025-12-31.md"),
            "transcript",
        )
        .await
        .unwrap();
        let lines = ContextSampler::new(dir.path().to_path_buf())
            .transcript_lines(
                &[sample("2026-01-01T00:00:01Z", "42")],
                "Kappa",
                timestamp("2026-01-02T02:00:00Z"),
                run_at("03:00"),
            )
            .await
            .unwrap();
        assert_eq!(lines, vec!["01:00:01  alice#42: Kappa nach Mitternacht"]);
    }

    #[tokio::test]
    async fn yesterday_before_rotation_still_uses_today() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write_bytes_async(
            b"20:00:00  alice#42: Kappa gestern noch offen\n",
            &dir.path().join("today.md"),
            "transcript",
        )
        .await
        .unwrap();
        let lines = ContextSampler::new(dir.path().to_path_buf())
            .transcript_lines(
                &[sample("2026-01-01T19:00:00Z", "42")],
                "Kappa",
                timestamp("2026-01-02T01:00:00Z"),
                run_at("03:00"),
            )
            .await
            .unwrap();
        assert_eq!(lines, vec!["20:00:00  alice#42: Kappa gestern noch offen"]);
    }

    #[tokio::test]
    async fn same_clock_line_on_wrong_day_does_not_win() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write_bytes_async(
            b"01:00:01  alice#42: Kappa richtige Rotation\n",
            &dir.path().join("2025-12-31.md"),
            "transcript",
        )
        .await
        .unwrap();
        atomic_write_bytes_async(
            b"01:00:01  alice#42: Kappa falsches Datum\n",
            &dir.path().join("today.md"),
            "transcript",
        )
        .await
        .unwrap();
        let lines = ContextSampler::new(dir.path().to_path_buf())
            .transcript_lines(
                &[sample("2026-01-01T00:00:01Z", "42")],
                "Kappa",
                timestamp("2026-01-02T02:00:00Z"),
                run_at("03:00"),
            )
            .await
            .unwrap();
        assert_eq!(lines, vec!["01:00:01  alice#42: Kappa richtige Rotation"]);
    }

    #[tokio::test]
    async fn same_second_samples_consume_distinct_emote_lines() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write_bytes_async(
            "01:00:01  alice#42: Kappa erste Nachricht\n\
             01:00:01  alice#42: ohne Emote\n\
             01:00:01  alice#42: Kappa zweite Nachricht\n"
                .as_bytes(),
            &dir.path().join("today.md"),
            "transcript",
        )
        .await
        .unwrap();
        let lines = ContextSampler::new(dir.path().to_path_buf())
            .transcript_lines(
                &[
                    sample("2026-01-01T00:00:01.001Z", "42"),
                    sample("2026-01-01T00:00:01.000Z", "42"),
                ],
                "Kappa",
                timestamp("2026-01-02T00:00:00Z"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            lines,
            vec![
                "01:00:01  alice#42: Kappa zweite Nachricht",
                "01:00:01  alice#42: Kappa erste Nachricht"
            ]
        );
    }
}
