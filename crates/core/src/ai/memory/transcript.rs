//! Append-as-you-go transcript file. Line format:
//!   `HH:MM:SS  <user>#<twitch-user-id>: <text>` (Berlin local time), or
//!   `HH:MM:SS  <user>: <text>` when no id is known (the bot's own replies).
//! The id is what lets the dreamer map a transcript login onto `users/<id>.md`
//! instead of guessing a number for it.
//! `today.md` is the open handle; ritual rotation renames it to
//! `<YYYY-MM-DD>.md` and opens a fresh `today.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr as _};
use jiff::Timestamp;
use jiff::civil::Date;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct TranscriptWriter {
    inner: Arc<Mutex<Inner>>,
    transcripts_dir: PathBuf,
}

struct Inner {
    handle: tokio::fs::File,
}

impl TranscriptWriter {
    pub async fn open(memories_dir: &Path) -> Result<Self> {
        let dir = memories_dir.join("transcripts");
        tokio::fs::create_dir_all(&dir)
            .await
            .wrap_err_with(|| format!("create_dir_all {}", dir.display()))?;
        let path = dir.join("today.md");
        let handle = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .wrap_err_with(|| format!("open transcripts/today.md at {}", path.display()))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { handle })),
            transcripts_dir: dir,
        })
    }

    pub async fn append_line(
        &self,
        ts: Timestamp,
        user: &str,
        user_id: Option<&str>,
        text: &str,
    ) -> Result<()> {
        let local = crate::util::to_berlin(ts);
        let safe_user = user.replace(['\n', '\r'], " ");
        let safe_text = text.replace(['\n', '\r'], " ");
        // Ids come from Twitch tags and are numeric, but a transport that ever
        // hands us something else must not be able to break the line format.
        let id_suffix = match user_id.filter(|id| crate::ai::memory::sanitize::is_valid_user_id(id))
        {
            Some(id) => format!("#{id}"),
            None => String::new(),
        };
        let line = format!(
            "{}  {}{}: {}\n",
            local.strftime("%H:%M:%S"),
            safe_user,
            id_suffix,
            safe_text
        );
        let mut g = self.inner.lock().await;
        g.handle
            .write_all(line.as_bytes())
            .await
            .wrap_err("write transcript")?;
        // tokio::fs::File buffers internally — flush so the dreamer ritual
        // (and tests reading the file directly) sees the line immediately.
        g.handle.flush().await.wrap_err("flush transcript")?;
        Ok(())
    }

    /// Close current `today.md`, rename to `<date>.md`, reopen empty `today.md`.
    pub async fn rotate_to(&self, date: Date) -> Result<PathBuf> {
        let mut g = self.inner.lock().await;
        if let Err(error) = g.handle.flush().await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(?error, "flush transcript before rotate failed");
        }
        let today = self.transcripts_dir.join("today.md");
        let dated = self
            .transcripts_dir
            .join(format!("{}.md", date.strftime("%Y-%m-%d")));
        tokio::fs::rename(&today, &dated)
            .await
            .wrap_err_with(|| format!("rotate {} -> {}", today.display(), dated.display()))?;
        let new_handle = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&today)
            .await
            .wrap_err("reopen today.md")?;
        g.handle = new_handle;
        Ok(dated)
    }

    pub fn dated_path_for(&self, date: Date) -> PathBuf {
        self.transcripts_dir
            .join(format!("{}.md", date.strftime("%Y-%m-%d")))
    }

    /// Delete dated transcripts (`<YYYY-MM-DD>.md`) older than `cutoff`.
    /// `today.md` and anything else not named like a date stay. Returns how
    /// many files were removed.
    pub async fn prune_before(&self, cutoff: Date) -> Result<usize> {
        let mut removed = 0;
        let mut entries = tokio::fs::read_dir(&self.transcripts_dir)
            .await
            .wrap_err("read_dir transcripts")?;
        while let Some(entry) = entries.next_entry().await.wrap_err("next_entry")? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(date) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<Date>().ok())
            else {
                continue;
            };
            if date < cutoff {
                tokio::fs::remove_file(&path)
                    .await
                    .wrap_err_with(|| format!("remove {}", path.display()))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_writes_line() {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("memories");
        tokio::fs::create_dir_all(memories.join("transcripts"))
            .await
            .unwrap();
        let w = TranscriptWriter::open(&memories).await.unwrap();
        let ts: Timestamp = "2026-04-28T10:42:11Z".parse().unwrap();
        w.append_line(ts, "alice", Some("12345"), "hi")
            .await
            .unwrap();
        let s = tokio::fs::read_to_string(memories.join("transcripts/today.md"))
            .await
            .unwrap();
        assert!(s.contains("alice#12345: hi"), "got {s}");
    }

    #[tokio::test]
    async fn rotate_renames_today_to_yesterday_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("memories");
        tokio::fs::create_dir_all(memories.join("transcripts"))
            .await
            .unwrap();
        let w = TranscriptWriter::open(&memories).await.unwrap();
        w.append_line(Timestamp::now(), "a", None, "x")
            .await
            .unwrap();
        let yday = jiff::civil::date(2026, 4, 27);
        let dest = w.rotate_to(yday).await.unwrap();
        assert_eq!(dest.file_name().unwrap().to_string_lossy(), "2026-04-27.md");
        assert!(dest.exists());
        assert!(memories.join("transcripts/today.md").exists()); // reopened empty
    }
}
