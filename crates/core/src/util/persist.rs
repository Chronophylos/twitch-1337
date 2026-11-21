//! Atomic file persistence helpers.
//!
//! Helpers serialize to RON, write the destination with extension replaced by
//! `ron.tmp`, then rename onto the target so concurrent readers see either the
//! old or the new file but never a torn write. Callers should pass a path
//! ending in `.ron`. "Atomic" here means no torn reads; it does NOT imply
//! crash-safety (no `fsync` is performed).

use std::path::{Path, PathBuf};

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

async fn write_bytes(bytes: &[u8], path: &Path) -> Result<()> {
    let tmp = path.with_extension("tmp");
    #[expect(
        clippy::disallowed_methods,
        reason = "this is the tmp half of tmp + rename"
    )]
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|source| AtomicPersistError::WriteTmp {
            path: tmp.clone(),
            source,
        })?;
    rename(&tmp, path).await
}

async fn save_ron<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    let tmp = path.with_extension("ron.tmp");
    let data = serialize(value)?;

    #[expect(
        clippy::disallowed_methods,
        reason = "this is the tmp half of tmp + rename"
    )]
    tokio::fs::write(&tmp, data.as_bytes())
        .await
        .map_err(|source| AtomicPersistError::WriteTmp {
            path: tmp.clone(),
            source,
        })?;

    rename(&tmp, path).await
}

async fn save_ron_private<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    let tmp = path.with_extension("ron.tmp");
    let data = serialize(value)?;
    write_private(&tmp, data.as_bytes())
        .await
        .map_err(|source| AtomicPersistError::WriteTmp {
            path: tmp.clone(),
            source,
        })?;
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
    // `mode` only applies when the file is created; a tmp left behind by a
    // crashed save keeps its old mode, so set it explicitly as well.
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
    tokio::fs::rename(tmp, path)
        .await
        .map_err(|source| AtomicPersistError::Rename {
            from: tmp.to_path_buf(),
            to: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!path.with_extension("ron.tmp").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_save_is_owner_only_over_world_readable_leftovers() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.ron");
        let tmp = path.with_extension("ron.tmp");
        // A target written by an older build and a tmp left by a crashed save.
        for file in [&path, &tmp] {
            #[expect(clippy::disallowed_methods, reason = "test seeds 0644 files")]
            std::fs::write(file, "old").unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let value = Sample {
            a: 2,
            b: "secret".into(),
        };
        atomic_save_ron_private_async(&value, &path).await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        let loaded: Sample = ron::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded, value);
        assert!(!tmp.exists());
    }
}
