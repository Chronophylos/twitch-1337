//! Atomic file persistence helpers.
//!
//! Helpers serialize to RON, write a unique sibling tmp file
//! (`.<name>.<pid>.<n>.tmp`), then rename onto the target so concurrent readers see either the
//! old or the new file but never a torn write. Callers should pass a path
//! ending in `.ron`. "Atomic" here means no torn reads; it does NOT imply
//! crash-safety (no `fsync` is performed).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use thiserror::Error;
use tracing::{Instrument as _, field::Empty, info_span};

/// Errors that can occur during atomic persistence operations.
#[derive(Debug, Error)]
pub enum AtomicPersistError {
    /// Failed to serialize value to RON format.
    #[error("Failed to serialize value to RON: {0}")]
    Serialization(#[from] ron::Error),

    /// Failed to write temporary file.
    #[error("Failed to write tmp file {path}: {source}")]
    WriteTmp {
        /// Path of the temporary file.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },

    /// Failed to rename temporary file to destination.
    #[error("Failed to rename {from} -> {to}: {source}")]
    Rename {
        /// Path of the source file.
        from: PathBuf,
        /// Path of the destination file.
        to: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
}

impl AtomicPersistError {
    /// Variant name, recorded as `error.type`.
    fn variant_name(&self) -> &'static str {
        match self {
            Self::Serialization(_) => "Serialization",
            Self::WriteTmp { .. } => "WriteTmp",
            Self::Rename { .. } => "Rename",
        }
    }
}

pub type Result<T> = std::result::Result<T, AtomicPersistError>;

fn serialize<T: Serialize>(value: &T) -> Result<String> {
    ron::ser::to_string_pretty(value, ron::ser::PrettyConfig::default())
        .map_err(AtomicPersistError::Serialization)
}

/// Async byte-level atomic write — does not require `Serialize`.
/// Writes `bytes` to a `.tmp` sibling then renames onto `path`.
///
/// `file` is the `bot.persist.file` span label. Unlike the RON helpers, which
/// label with the file name, byte callers write files named after open-ended
/// input (memory files per user), so they pass a fixed kind instead.
pub async fn atomic_write_bytes_async(
    bytes: &[u8],
    path: &Path,
    file: &'static str,
) -> std::result::Result<(), AtomicPersistError> {
    traced(file, write_bytes(bytes, path)).await
}

/// Async variant — uses `tokio::fs` so it does not block the runtime.
/// Serializes with `ron::to_string` (compact) to match the existing tracker
/// behaviour; switch to pretty if a call-site needs human inspection.
pub async fn atomic_save_ron_async<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    traced(file_label(path), save_ron(value, path)).await
}

/// [`atomic_save_ron_async`] for files holding credentials (OAuth tokens,
/// session refresh tokens): on unix the file ends up owner-only (`0600`)
/// regardless of the process umask.
///
/// The mode is set on the tmp file before any data is written, and the
/// rename carries it onto `path`, so a file left `0644` by an older build is
/// tightened on its next save.
pub async fn atomic_save_ron_private_async<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    traced(file_label(path), save_ron_private(value, path)).await
}

/// Run one write inside a `persist.write` span labelled with `file`.
async fn traced(file: &str, write: impl Future<Output = Result<()>>) -> Result<()> {
    let span = info_span!(
        "persist.write",
        bot.persist.file = file,
        error.type = Empty,
        otel.status_code = Empty,
    );
    let result = write.instrument(span.clone()).await;
    if let Err(error) = &result {
        span.record("error.type", error.variant_name());
        span.record("otel.status_code", "ERROR");
    }
    result
}

/// `bot.persist.file` for a RON state file: its file name. Every RON file
/// has a fixed name, so this stays a bounded set; never the full path.
fn file_label(path: &Path) -> &str {
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
}

/// Sibling tmp path unique per call, so concurrent saves to one `path` never
/// share (and tear) a tmp file.
fn unique_tmp(path: &Path) -> PathBuf {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()))
}

/// Best-effort removal of a tmp file left by a failed write or rename.
async fn cleanup_tmp(tmp: &Path) {
    if let Err(e) = tokio::fs::remove_file(tmp).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(error = %e, path = %tmp.display(), "failed to remove tmp file");
    }
}

/// Remove orphaned tmp files from `dir` (non-recursive, best-effort).
///
/// Call once at startup, before any writer runs. Removes new-scheme tmp files
/// (`.<name>.<pid>.<n>.tmp`) of other processes, which a crash or cancelled
/// save leaves behind, and legacy `*.ron.tmp` files from before unique tmp
/// names. A tmp of the current process is never touched.
pub fn sweep_stale_tmp(dir: &Path) {
    sweep(dir, false);
}

/// [`sweep_stale_tmp`] for a directory written only by
/// [`atomic_write_bytes_async`] (the memory store's `memories/`, `users/`,
/// `scratch/`): additionally removes legacy `<stem>.tmp` files. Never use it
/// on a directory that holds other `*.tmp` files.
pub fn sweep_stale_tmp_bytes_dir(dir: &Path) {
    sweep(dir, true);
}

fn sweep(dir: &Path, legacy_bare: bool) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(error = %e, dir = %dir.display(), "cannot scan for stale tmp files");
            }
            return;
        }
    };
    let me = std::process::id();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_stale_tmp(name, me, legacy_bare) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::debug!(error = %e, path = %entry.path().display(), "failed to remove stale tmp file");
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, dir = %dir.display(), "removed stale tmp files");
    }
}

fn is_stale_tmp(name: &str, me: u32, legacy_bare: bool) -> bool {
    let Some(stem) = name.strip_suffix(".tmp") else {
        return false;
    };
    if let Some(rest) = stem.strip_prefix('.')
        && let Some((rest, n)) = rest.rsplit_once('.')
        && let Some((_, pid)) = rest.rsplit_once('.')
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
        && let Ok(pid) = pid.parse::<u32>()
    {
        return pid != me;
    }
    stem.ends_with(".ron") || (legacy_bare && !stem.starts_with('.'))
}

async fn write_bytes(bytes: &[u8], path: &Path) -> Result<()> {
    let tmp = unique_tmp(path);
    #[expect(
        clippy::disallowed_methods,
        reason = "this is the tmp half of tmp + rename"
    )]
    if let Err(source) = tokio::fs::write(&tmp, bytes).await {
        cleanup_tmp(&tmp).await;
        return Err(AtomicPersistError::WriteTmp { path: tmp, source });
    }
    rename(&tmp, path).await
}

async fn save_ron<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    let tmp = unique_tmp(path);
    let data = serialize(value)?;

    #[expect(
        clippy::disallowed_methods,
        reason = "this is the tmp half of tmp + rename"
    )]
    if let Err(source) = tokio::fs::write(&tmp, data.as_bytes()).await {
        cleanup_tmp(&tmp).await;
        return Err(AtomicPersistError::WriteTmp { path: tmp, source });
    }

    rename(&tmp, path).await
}

async fn save_ron_private<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    let tmp = unique_tmp(path);
    let data = serialize(value)?;
    if let Err(source) = write_private(&tmp, data.as_bytes()).await {
        cleanup_tmp(&tmp).await;
        return Err(AtomicPersistError::WriteTmp { path: tmp, source });
    }
    rename(&tmp, path).await
}

/// Write `bytes` to `tmp`, owner read/write only on unix.
async fn write_private(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(tmp).await?;
    // `mode` only applies when the file is created; set it explicitly as well.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    file.write_all(bytes).await?;
    file.flush().await
}

async fn rename(tmp: &Path, path: &Path) -> Result<()> {
    if let Err(source) = tokio::fs::rename(tmp, path).await {
        cleanup_tmp(tmp).await;
        return Err(AtomicPersistError::Rename {
            from: tmp.to_path_buf(),
            to: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_stale_tmp_and_keeps_live_and_unrelated() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let foreign_pid = me.wrapping_add(1);
        let foreign = format!(".token.ron.{foreign_pid}.3.tmp");
        let current = format!(".token.ron.{me}.4.tmp");
        for name in [foreign.as_str(), current.as_str(), "x.ron.tmp", "notes.tmp"] {
            #[expect(clippy::disallowed_methods, reason = "test seeds files")]
            std::fs::write(dir.path().join(name), "secret").unwrap();
        }

        sweep_stale_tmp(dir.path());

        assert!(!dir.path().join(&foreign).exists());
        assert!(!dir.path().join("x.ron.tmp").exists());
        assert!(dir.path().join(&current).exists());
        assert!(dir.path().join("notes.tmp").exists());

        // Byte-file dirs additionally drop legacy `<stem>.tmp`.
        sweep_stale_tmp_bytes_dir(dir.path());
        assert!(!dir.path().join("notes.tmp").exists());
        assert!(dir.path().join(&current).exists());
    }

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[tokio::test]
    async fn async_writes_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.ron");
        let value = Sample {
            a: 1,
            b: "x".into(),
        };
        atomic_save_ron_async(&value, &path).await.unwrap();
        let loaded: Sample =
            ron::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert_eq!(loaded, value);
        assert_eq!(tmp_files(dir.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_save_is_owner_only_over_world_readable_target() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.ron");
        // A target written by an older build.
        #[expect(clippy::disallowed_methods, reason = "test seeds a 0644 file")]
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let value = Sample {
            a: 2,
            b: "secret".into(),
        };
        atomic_save_ron_private_async(&value, &path).await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        let loaded: Sample = ron::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded, value);
        assert_eq!(tmp_files(dir.path()), Vec::<String>::new());
    }

    fn tmp_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[tokio::test]
    async fn concurrent_saves_to_same_path_do_not_tear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.ron");
        let mut handles = Vec::new();
        for i in 0..16u32 {
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                let value = vec![i; 1000];
                atomic_save_ron_async(&value, &path).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let loaded: Vec<u32> = ron::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.len(), 1000);
        assert!(loaded.iter().all(|v| *v == loaded[0]) && loaded[0] < 16);
        assert_eq!(tmp_files(dir.path()), Vec::<String>::new());
    }
}
