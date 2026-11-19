//! Filesystem layer for v2 memory: read/write/list under per-path mutex,
//! atomic tmp+rename, byte caps, soul seeding, v1 disposal.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr as _, eyre};
use jiff::Timestamp;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::ai::memory::frontmatter;
use crate::ai::memory::sanitize::{SlugError, normalize_display_name};
use crate::ai::memory::types::{Caps, FileKind, Frontmatter, MemoryFile};
use crate::settings::SettingsHandle;
use crate::util::persist::atomic_write_bytes_async;

/// On-disk modification time, milliseconds since the UNIX epoch. Used as a
/// lost-update token by the dashboard memory editor.
pub type Mtime = u64;

pub use crate::ai::memory::sanitize::validate_scratch_slug;

fn slug_write_err(err: SlugError) -> WriteError {
    match err {
        SlugError::Invalid => WriteError::InvalidSlug,
        SlugError::Reserved => WriteError::ReservedSlug,
    }
}

/// Outcome of `MemoryStore::write_with_guard`: either the write went
/// through (caller can read the new mtime back) or the on-disk mtime
/// disagreed with the caller's expected token, in which case the dashboard
/// renders the conflict template with the current body for manual merge.
#[derive(Debug)]
pub enum WriteOutcome {
    Written {
        new_mtime: Mtime,
    },
    Conflict {
        current_body: String,
        current_mtime: Mtime,
    },
}

/// Per-write frontmatter overrides for `write_with_guard`. `Some(non_empty)`
/// replaces the on-disk value; `Some("")` and `None` both preserve the
/// prior value (matches `write`'s existing empty-string filtering, so
/// untouched form inputs round-trip safely through the conflict resubmit
/// path which doesn't carry these fields).
#[derive(Default, Debug, Clone)]
pub struct FrontmatterOverride {
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub created_by: Option<String>,
    /// `Some` replaces the whole links map (empty clears it), `None` preserves.
    /// User files only.
    pub links: Option<BTreeMap<String, String>>,
}

/// Identity fields a caller can stamp on a user file. Every field is
/// preserve-on-`None`: a mod or the dreamer writing someone else's sheet
/// supplies only what it actually knows and can't clobber the rest.
/// Ignored entirely for SOUL/LORE/scratch, which have no subject.
#[derive(Default, Debug, Clone, Copy)]
pub struct UserIdentity<'a> {
    pub username: Option<&'a str>,
    pub display_name: Option<&'a str>,
    /// Display form ("He/Him"), resolved from pronouns.alejo.io by the caller.
    pub pronouns: Option<&'a str>,
}

impl<'a> UserIdentity<'a> {
    pub fn of(username: &'a str, display_name: &'a str) -> Self {
        Self {
            username: Some(username),
            display_name: Some(display_name),
            pronouns: None,
        }
    }
}

const SOUL_SEED: &str = twitch_1337_data::prompts::SEED_SOUL;

#[derive(Clone)]
pub struct MemoryStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    root: PathBuf,         // $DATA_DIR
    memories_dir: PathBuf, // $DATA_DIR/memories
    settings: SettingsHandle,
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Serialises new scratch-file creation so the `max_scratch_files` cap
    /// can't be smuggled past by two concurrent writes to two distinct new
    /// slugs. Held only across the count-check + write path inside
    /// `write_scratch`.
    scratch_create_lock: Mutex<()>,
}

impl MemoryStore {
    pub async fn open(data_dir: &Path, settings: SettingsHandle) -> Result<Self> {
        let memories_dir = data_dir.join("memories");

        // ADR-0003: `state/` was renamed to `scratch/`. This MUST run before
        // the create_dir_all loop below — that loop would otherwise create an
        // empty `scratch/`, making the `!exists` guard false, silently skipping
        // the rename and stranding every existing note in `state/`.
        // Idempotent and self-clearing, so no sentinel is needed.
        let legacy_scratch = memories_dir.join("state");
        let scratch_dir = memories_dir.join("scratch");
        if tokio::fs::try_exists(&legacy_scratch)
            .await
            .unwrap_or(false)
        {
            if tokio::fs::try_exists(&scratch_dir).await.unwrap_or(false) {
                // Both present: refuse to merge or clobber, but say so — a
                // silent skip leaves notes stranded with no way to notice.
                warn!(
                    legacy = %legacy_scratch.display(),
                    "legacy memories/state/ present alongside memories/scratch/; \
                     not migrated (ADR-0003). Reconcile by hand and remove state/."
                );
            } else {
                tokio::fs::rename(&legacy_scratch, &scratch_dir)
                    .await
                    .wrap_err("migrate memories/state -> memories/scratch")?;
                info!("Renamed memories/state -> memories/scratch (ADR-0003)");
            }
        }

        for p in [
            &memories_dir,
            &memories_dir.join("users"),
            &scratch_dir,
            &memories_dir.join("transcripts"),
        ] {
            tokio::fs::create_dir_all(p)
                .await
                .wrap_err_with(|| format!("create_dir_all {}", p.display()))?;
        }

        // v1 disposal.
        let v1 = data_dir.join("ai_memory.ron");
        if tokio::fs::try_exists(&v1).await.unwrap_or(false) {
            let ts = Timestamp::now().as_second();
            let dest = data_dir.join(format!("ai_memory.ron.discarded-{ts}"));
            match tokio::fs::rename(&v1, &dest).await {
                Ok(()) => {
                    info!(target = %dest.display(), "Renamed v1 ai_memory.ron — v2 starts fresh");
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    warn!(?error, target = %dest.display(), "Failed to rename v1 ai_memory.ron");
                }
            }
        }

        // SOUL.md seed.
        let soul_path = memories_dir.join("SOUL.md");
        if !tokio::fs::try_exists(&soul_path).await.unwrap_or(false) {
            let fm = Frontmatter {
                updated_at: Timestamp::now(),
                username: None,
                display_name: None,
                created_by: None,
                pronouns: None,
                links: BTreeMap::new(),
            };
            let raw = frontmatter::emit(&fm, SOUL_SEED);
            atomic_write_bytes_async(raw.as_bytes(), &soul_path)
                .await
                .wrap_err("seed SOUL.md")?;
        }
        // LORE.md seed (empty body).
        let lore_path = memories_dir.join("LORE.md");
        if !tokio::fs::try_exists(&lore_path).await.unwrap_or(false) {
            let fm = Frontmatter {
                updated_at: Timestamp::now(),
                username: None,
                display_name: None,
                created_by: None,
                pronouns: None,
                links: BTreeMap::new(),
            };
            atomic_write_bytes_async(frontmatter::emit(&fm, "").as_bytes(), &lore_path)
                .await
                .wrap_err("seed LORE.md")?;
        }
        Ok(Self {
            inner: Arc::new(StoreInner {
                root: data_dir.to_path_buf(),
                memories_dir,
                settings,
                locks: Mutex::new(HashMap::new()),
                scratch_create_lock: Mutex::new(()),
            }),
        })
    }

    /// Read the current byte caps live from the settings handle. Lock-free
    /// (`ArcSwap::load` is a pointer swap) so safe to call on every operation.
    pub fn caps(&self) -> Caps {
        Caps::from_settings(&self.inner.settings.load())
    }

    pub fn memories_dir(&self) -> &Path {
        &self.inner.memories_dir
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    async fn lock_for(&self, rel: &Path) -> Arc<Mutex<()>> {
        let mut g = self.inner.locks.lock().await;
        g.entry(rel.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Read Scratch by Stable Slug. Missing → empty body, default frontmatter.
    /// Invalid / reserved / dated slugs return [`WriteError`] without touching disk.
    pub async fn read_scratch(&self, slug: &str) -> Result<MemoryFile, WriteError> {
        validate_scratch_slug(slug).map_err(slug_write_err)?;
        self.read_kind(&FileKind::Scratch {
            slug: slug.to_string(),
        })
        .await
        .map_err(WriteError::Io)
    }

    /// Read a single file by `FileKind`. Missing → empty body, default frontmatter.
    /// Scratch slugs are validated before any filesystem access.
    pub async fn read_kind(&self, kind: &FileKind) -> Result<MemoryFile> {
        if let FileKind::Scratch { slug } = kind {
            validate_scratch_slug(slug).map_err(|e| eyre!("{}", slug_write_err(e)))?;
        }
        let rel = kind.relative_path();
        let abs = self.inner.memories_dir.join(&rel);
        match tokio::fs::read_to_string(&abs).await {
            Ok(raw) => {
                let (fm, body) = frontmatter::parse(&raw)
                    .wrap_err_with(|| format!("parse {}", rel.display()))?;
                Ok(MemoryFile {
                    kind: kind.clone(),
                    frontmatter: fm,
                    body,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MemoryFile {
                kind: kind.clone(),
                frontmatter: Frontmatter {
                    updated_at: Timestamp::now(),
                    username: None,
                    display_name: None,
                    created_by: None,
                    pronouns: None,
                    links: BTreeMap::new(),
                },
                body: String::new(),
            }),
            Err(e) => Err(eyre!(e)),
        }
    }

    /// Write a SOUL/LORE/user file. For user files, [`UserIdentity`] is
    /// persisted in the frontmatter, preserve-on-`None` per field.
    pub async fn write(
        &self,
        kind: &FileKind,
        body: &str,
        ident: UserIdentity<'_>,
    ) -> Result<(), WriteError> {
        self.write_inner(kind, body, ident, None).await
    }

    /// `links_override`: `Some` replaces the whole `links` map (an empty map
    /// clears it), `None` preserves what's on disk. Only the dashboard sets
    /// it — no chat-turn tool can reach links.
    async fn write_inner(
        &self,
        kind: &FileKind,
        body: &str,
        ident: UserIdentity<'_>,
        links_override: Option<BTreeMap<String, String>>,
    ) -> Result<(), WriteError> {
        let limit = self.caps().limit_for(kind);

        let rel = kind.relative_path();
        let abs = self.inner.memories_dir.join(&rel);
        let lock = self.lock_for(&rel).await;
        let _g = lock.lock().await;

        // Build the emitted file inside the lock so the cap covers the *full*
        // on-disk content (frontmatter + body). Doing this before acquiring
        // the lock would let two concurrent writes both pass a body-only cap
        // check and then race the actual write.
        let (username, display_name, pronouns, links) = if matches!(kind, FileKind::User { .. }) {
            let username = ident.username.map(str::to_string).filter(|s| !s.is_empty());
            let display_name = ident
                .display_name
                .map(normalize_display_name)
                .filter(|s| !s.is_empty());
            let pronouns = ident
                .pronouns
                .map(normalize_display_name)
                .filter(|s| !s.is_empty());
            // For user files, preserve any prior field the caller didn't
            // supply — protects mod/dreamer writes from clobbering identity or
            // links they don't have on hand. Links are never passed by a
            // chat-turn write, so this read is what keeps them alive.
            let prior = tokio::fs::read_to_string(&abs)
                .await
                .ok()
                .and_then(|raw| frontmatter::parse(&raw).ok().map(|(fm, _)| fm));
            (
                // Both identity keys are always emitted, blank when still
                // unknown, so a sheet the dreamer created for a third party
                // shows an empty slot to fill later instead of no slot at all.
                // `parse` maps "" back to `None`, so a blank round-trips as
                // "unknown" and never clobbers.
                username
                    .or_else(|| prior.as_ref().and_then(|fm| fm.username.clone()))
                    .or_else(|| Some(String::new())),
                display_name
                    .or_else(|| prior.as_ref().and_then(|fm| fm.display_name.clone()))
                    .or_else(|| Some(String::new())),
                // Pronouns get no blank placeholder: an empty `pronouns:` key
                // would read as a claim that the user declared none, when it
                // only means the service was never asked.
                pronouns.or_else(|| prior.as_ref().and_then(|fm| fm.pronouns.clone())),
                links_override.unwrap_or_else(|| prior.map(|fm| fm.links).unwrap_or_default()),
            )
        } else {
            (None, None, None, BTreeMap::new())
        };

        let fm = Frontmatter {
            updated_at: Timestamp::now(),
            username,
            display_name,
            created_by: None,
            pronouns,
            links,
        };
        let raw = frontmatter::emit(&fm, body);
        if raw.len() > limit {
            tracing::warn!(
                path = %rel.display(),
                limit,
                actual = raw.len(),
                "memory write rejected, over byte cap"
            );
            return Err(WriteError::Full {
                limit,
                actual: raw.len(),
            });
        }

        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| WriteError::Io(eyre!(e)))?;
        }
        // ponytail: single-slot backup, so one bad dreamer run is recoverable.
        // Rotate to N versions only if one night back turns out to be too little.
        let bak = abs.with_extension("md.bak");
        match tokio::fs::rename(&abs, &bak).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => warn!(?error, path = %bak.display(), "memory .bak rotation failed"),
        }
        atomic_write_bytes_async(raw.as_bytes(), &abs)
            .await
            .map_err(|e| WriteError::Io(eyre!(e)))?;
        Ok(())
    }

    pub async fn write_scratch(
        &self,
        kind: &FileKind,
        body: &str,
        creator_user_id: Option<&str>,
    ) -> Result<(), WriteError> {
        self.write_scratch_inner(kind, body, creator_user_id, None)
            .await
    }

    /// `created_by_override`: `Some(non_empty)` replaces the on-disk
    /// `created_by`; `Some("")` and `None` both fall back to preserve-or-init.
    async fn write_scratch_inner(
        &self,
        kind: &FileKind,
        body: &str,
        creator_user_id: Option<&str>,
        created_by_override: Option<&str>,
    ) -> Result<(), WriteError> {
        let FileKind::Scratch { slug } = kind else {
            return Err(WriteError::Io(eyre!(
                "write_scratch called on non-scratch kind"
            )));
        };
        // Validate before touching the filesystem — the AI tool, the
        // dashboard, and the dreamer all funnel through this method, so the
        // slug rule lives here rather than at any one call site.
        validate_scratch_slug(slug).map_err(slug_write_err)?;
        let limit = self.caps().scratch_bytes;

        let rel = kind.relative_path();
        let abs = self.inner.memories_dir.join(&rel);
        let lock = self.lock_for(&rel).await;
        let _g = lock.lock().await;

        let prior_raw = tokio::fs::read_to_string(&abs).await.ok();
        let is_new = prior_raw.is_none();
        let prior_created_by = prior_raw
            .as_deref()
            .and_then(|raw| frontmatter::parse(raw).ok())
            .and_then(|(fm, _)| fm.created_by);
        let created_by = match created_by_override {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            _ => prior_created_by.or_else(|| creator_user_id.map(std::string::ToString::to_string)),
        };

        let fm = Frontmatter {
            updated_at: Timestamp::now(),
            username: None,
            display_name: None,
            created_by,
            pronouns: None,
            links: BTreeMap::new(),
        };
        let raw = frontmatter::emit(&fm, body);
        if raw.len() > limit {
            tracing::warn!(
                path = %rel.display(),
                limit,
                actual = raw.len(),
                "memory write rejected, over byte cap"
            );
            return Err(WriteError::Full {
                limit,
                actual: raw.len(),
            });
        }

        // For new files only, serialise the count-check + write through the
        // global scratch-create lock so two new-slug writes can't both pass.
        let _create_g = if is_new {
            Some(self.inner.scratch_create_lock.lock().await)
        } else {
            None
        };
        if is_new {
            let count = count_scratch_files(&self.inner.memories_dir).await?;
            if count >= self.caps().max_scratch_files {
                return Err(WriteError::ScratchFull);
            }
        }

        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| WriteError::Io(eyre!(e)))?;
        }
        atomic_write_bytes_async(raw.as_bytes(), &abs)
            .await
            .map_err(|e| WriteError::Io(eyre!(e)))?;
        Ok(())
    }

    /// Returns the on-disk modification time of `kind` as milliseconds since
    /// the UNIX epoch. Missing files return `0` so the editor can render a
    /// new-document mtime token without a separate "exists?" probe.
    ///
    /// Used by the dashboard editor (Task 5) to embed an `mtime` token in
    /// the form so a subsequent save can detect lost-update conflicts.
    pub async fn current_mtime(&self, kind: &FileKind) -> Result<Mtime, WriteError> {
        let abs = self.inner.memories_dir.join(kind.relative_path());
        match tokio::fs::metadata(&abs).await {
            Ok(meta) => {
                let modified = meta
                    .modified()
                    .map_err(|e| WriteError::Io(eyre!("modified: {e}")))?;
                let dur = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| WriteError::Io(eyre!("epoch: {e}")))?;
                u64::try_from(dur.as_millis())
                    .map_err(|e| WriteError::Io(eyre!("mtime overflow: {e}")))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(WriteError::Io(eyre!("metadata: {e}"))),
        }
    }

    /// Lost-update-guarded write used by the dashboard memory editor.
    ///
    /// `expected = Some(mtime)` re-reads the on-disk mtime under the
    /// per-path mutex and aborts with `WriteOutcome::Conflict` (returning
    /// the current body so the conflict template can show both versions
    /// side-by-side) when the disk state doesn't match.
    /// `expected = None` is unconditional — used by the dreamer ritual and
    /// the AI `write_file` tool, which serialise through the same per-path
    /// mutex map and don't carry a token.
    ///
    /// TOCTOU note: we drop the outer guard between the mtime check and
    /// the inner `write` / `write_scratch` (which take their own per-path
    /// lock). In that gap an unconditional write (AI tool or dreamer
    /// ritual) can land between the dashboard user's check and their
    /// write — the dashboard write then silently overwrites that change
    /// without surfacing a conflict. Acceptable for v1 since every write
    /// path serialises on the same per-path mutex map and the gap is
    /// short; the dashboard user's intent is always preserved.
    pub async fn write_with_guard(
        &self,
        kind: FileKind,
        body: &str,
        expected: Option<Mtime>,
        fm_override: FrontmatterOverride,
    ) -> Result<WriteOutcome, WriteError> {
        let rel = kind.relative_path();
        let lock = self.lock_for(&rel).await;
        let g = lock.lock().await;

        if let Some(exp) = expected {
            let current = self.current_mtime(&kind).await?;
            if current != exp {
                let body = self
                    .read_kind(&kind)
                    .await
                    .map(|f| f.body)
                    .unwrap_or_default();
                return Ok(WriteOutcome::Conflict {
                    current_body: body,
                    current_mtime: current,
                });
            }
        }

        drop(g);
        match &kind {
            FileKind::Scratch { slug } => {
                self.write_scratch_inner(
                    &FileKind::Scratch { slug: slug.clone() },
                    body,
                    None,
                    fm_override.created_by.as_deref(),
                )
                .await?;
            }
            FileKind::User { .. } => {
                self.write_inner(
                    &kind,
                    body,
                    UserIdentity {
                        username: fm_override.username.as_deref(),
                        display_name: fm_override.display_name.as_deref(),
                        // Pronouns are service-sourced, never dashboard-typed.
                        pronouns: None,
                    },
                    fm_override.links,
                )
                .await?;
            }
            FileKind::Soul | FileKind::Lore => {
                self.write(&kind, body, UserIdentity::default()).await?;
            }
        }
        let new_mtime = self.current_mtime(&kind).await?;
        Ok(WriteOutcome::Written { new_mtime })
    }

    pub async fn delete_scratch(&self, slug: &str) -> Result<(), WriteError> {
        // Same Stable Slug rule as write/read — every slug-taking call enforces it.
        validate_scratch_slug(slug).map_err(slug_write_err)?;
        let rel = PathBuf::from(format!("scratch/{slug}.md"));
        let abs = self.inner.memories_dir.join(&rel);
        let lock = self.lock_for(&rel).await;
        let _g = lock.lock().await;
        match tokio::fs::remove_file(&abs).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(WriteError::Io(eyre!(e))),
        }
    }

    /// Forget: delete one user's Character Sheet (`users/<id>.md`) and
    /// nothing else. Mentions in the Lore, other sheets and Transcripts stay.
    /// Returns `Ok(false)` when no sheet existed. A non-numeric id is
    /// rejected before any filesystem access, so it can't escape `users/`.
    pub async fn forget_user(&self, user_id: &str) -> Result<bool, WriteError> {
        if user_id.is_empty() || !user_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(WriteError::Io(eyre!("invalid user id")));
        }
        let rel = FileKind::User {
            user_id: user_id.to_owned(),
        }
        .relative_path();
        let abs = self.inner.memories_dir.join(&rel);
        let lock = self.lock_for(&rel).await;
        let _g = lock.lock().await;
        match tokio::fs::remove_file(&abs).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(WriteError::Io(eyre!(e))),
        }
    }

    pub async fn list_users(&self) -> Result<Vec<MemoryFile>> {
        self.list_in_subdir("users", |stem| FileKind::User {
            user_id: stem.into(),
        })
        .await
    }

    /// Ids of every user sheet on disk, from directory entries only — no file
    /// is opened or parsed. Callers use it as the write allowlist for
    /// `users/<id>.md`, which runs on every chat turn and must stay cheap.
    pub async fn list_user_ids(&self) -> Result<std::collections::BTreeSet<String>> {
        let dir = self.inner.memories_dir.join("users");
        let mut out = std::collections::BTreeSet::new();
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .wrap_err_with(|| format!("read_dir {}", dir.display()))?;
        while let Some(e) = entries.next_entry().await.wrap_err("next_entry")? {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string());
            }
        }
        Ok(out)
    }

    pub async fn list_scratch(&self) -> Result<Vec<MemoryFile>> {
        self.list_in_subdir("scratch", |stem| FileKind::Scratch { slug: stem.into() })
            .await
    }

    async fn list_in_subdir(
        &self,
        sub: &str,
        kind_for: impl Fn(&str) -> FileKind,
    ) -> Result<Vec<MemoryFile>> {
        let dir = self.inner.memories_dir.join(sub);
        let mut out = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .wrap_err_with(|| format!("read_dir {}", dir.display()))?;
        while let Some(e) = entries.next_entry().await.wrap_err("next_entry")? {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let kind = kind_for(&stem);
            out.push(self.read_kind(&kind).await?);
        }
        out.sort_by_key(|f| std::cmp::Reverse(f.frontmatter.updated_at));
        Ok(out)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("file_full")]
    Full { limit: usize, actual: usize },
    #[error("scratch_full")]
    ScratchFull,
    #[error("invalid_slug")]
    InvalidSlug,
    #[error("reserved_slug")]
    ReservedSlug,
    #[error("io: {0}")]
    Io(#[from] eyre::Report),
}

/// Count `*.md` entries in `memories/scratch/`. Used by `write_scratch` under
/// the scratch-create lock to enforce `max_scratch_files`.
async fn count_scratch_files(memories_dir: &Path) -> Result<usize, WriteError> {
    let dir = memories_dir.join("scratch");
    let mut entries = tokio::fs::read_dir(&dir)
        .await
        .map_err(|e| WriteError::Io(eyre!("read_dir scratch/: {e}")))?;
    let mut n = 0usize;
    while let Some(e) = entries
        .next_entry()
        .await
        .map_err(|e| WriteError::Io(eyre!("read_dir next: {e}")))?
    {
        if e.path().extension().and_then(|s| s.to_str()) == Some("md") {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ai::memory::types::Caps;
    use crate::settings::{Settings, SettingsHandle};

    fn test_handle() -> SettingsHandle {
        SettingsHandle::from_settings(Settings::compiled_defaults())
    }

    fn handle_with_caps(caps: Caps) -> SettingsHandle {
        let mut s = Settings::compiled_defaults();
        s.ai.memory.soul_bytes = caps.soul_bytes;
        s.ai.memory.lore_bytes = caps.lore_bytes;
        s.ai.memory.user_bytes = caps.user_bytes;
        s.ai.memory.scratch_bytes = caps.scratch_bytes;
        s.ai.memory.max_scratch_files = caps.max_scratch_files;
        SettingsHandle::from_settings(s)
    }

    /// ADR-0003 migration. This is the regression guard for the ordering trap:
    /// if the rename is ever moved below the `create_dir_all` loop in `open`,
    /// that loop creates an empty `scratch/` first, the `!exists` guard goes
    /// false, the rename is skipped, and every note is stranded in `state/`.
    /// A fresh-tempdir test cannot catch that — only a pre-seeded `state/` can.
    #[tokio::test]
    async fn open_migrates_legacy_state_dir_to_scratch() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("memories/state");
        tokio::fs::create_dir_all(&legacy).await.unwrap();
        tokio::fs::write(
            legacy.join("quiz.md"),
            "---\nupdated_at: 2026-07-16T00:00:00Z\n---\nscore: 7",
        )
        .await
        .unwrap();

        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();

        assert!(
            !dir.path().join("memories/state").exists(),
            "legacy state/ must be gone after migration"
        );
        let migrated = dir.path().join("memories/scratch/quiz.md");
        assert!(migrated.exists(), "note must land in scratch/");
        let body = store
            .read_kind(&FileKind::Scratch {
                slug: "quiz".into(),
            })
            .await
            .unwrap()
            .body;
        assert!(body.contains("score: 7"), "body must survive intact");
    }

    #[tokio::test]
    async fn open_migration_is_noop_when_scratch_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("memories/state");
        let scratch = dir.path().join("memories/scratch");
        tokio::fs::create_dir_all(&legacy).await.unwrap();
        tokio::fs::create_dir_all(&scratch).await.unwrap();
        tokio::fs::write(
            scratch.join("keep.md"),
            "---\nupdated_at: 2026-07-16T00:00:00Z\n---\nkeep me",
        )
        .await
        .unwrap();

        MemoryStore::open(dir.path(), test_handle()).await.unwrap();

        // Must not clobber an existing scratch/ — the legacy dir is left alone
        // for an operator to reconcile rather than silently merged or lost.
        assert!(
            tokio::fs::read_to_string(scratch.join("keep.md"))
                .await
                .unwrap()
                .contains("keep me")
        );
        assert!(dir.path().join("memories/state").exists());
    }

    #[tokio::test]
    async fn open_creates_tree_and_seeds_soul() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        assert!(dir.path().join("memories/SOUL.md").exists());
        assert!(dir.path().join("memories/users").is_dir());
        assert!(dir.path().join("memories/scratch").is_dir());
        assert!(dir.path().join("memories/transcripts").is_dir());

        let soul = store.read_kind(&FileKind::Soul).await.unwrap();
        assert!(soul.body.contains("Aurora"));
    }

    #[tokio::test]
    async fn open_renames_v1_store_when_present() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("ai_memory.ron"), b"v1 garbage")
            .await
            .unwrap();
        MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut found = false;
        while let Some(e) = entries.next_entry().await.unwrap() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("ai_memory.ron.discarded-") {
                found = true;
            }
        }
        assert!(found, "expected discarded v1 store");
    }

    #[tokio::test]
    async fn write_user_file_persists_and_caps_apply() {
        let dir = tempfile::tempdir().unwrap();
        // Cap covers the *full* on-disk file (frontmatter + body), so it must
        // be larger than the ~80B frontmatter overhead but small enough to
        // still reject a 4 KiB body.
        let store = MemoryStore::open(
            dir.path(),
            handle_with_caps(Caps {
                user_bytes: 256,
                ..Caps::default()
            }),
        )
        .await
        .unwrap();
        let kind = FileKind::User {
            user_id: "12".into(),
        };

        store
            .write(
                &kind,
                "small body",
                UserIdentity::of("alicepleb", "AlicePleb"),
            )
            .await
            .unwrap();
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.body.trim(), "small body");
        assert_eq!(mf.frontmatter.username.as_deref(), Some("alicepleb"));
        assert_eq!(mf.frontmatter.display_name.as_deref(), Some("AlicePleb"));

        let huge = "x".repeat(4096);
        let err = store
            .write(&kind, &huge, UserIdentity::of("alicepleb", "AlicePleb"))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "file_full");
    }

    #[tokio::test]
    async fn write_scratch_full_enforced_in_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(
            dir.path(),
            handle_with_caps(Caps {
                max_scratch_files: 1,
                ..Caps::default()
            }),
        )
        .await
        .unwrap();
        store
            .write_scratch(
                &FileKind::Scratch {
                    slug: "first".into(),
                },
                "x",
                Some("1"),
            )
            .await
            .unwrap();
        // Overwriting the same slug must still succeed at the cap.
        store
            .write_scratch(
                &FileKind::Scratch {
                    slug: "first".into(),
                },
                "y",
                Some("1"),
            )
            .await
            .unwrap();
        // A second distinct slug must fail.
        let err = store
            .write_scratch(
                &FileKind::Scratch {
                    slug: "second".into(),
                },
                "x",
                Some("1"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::ScratchFull));
    }

    #[tokio::test]
    async fn display_name_is_normalised_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::User {
            user_id: "99".into(),
        };
        let dirty = "ali\u{200B}ce\nx";
        store
            .write(&kind, "body", UserIdentity::of("alicex", dirty))
            .await
            .unwrap();
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.frontmatter.display_name.as_deref(), Some("alicex"));
    }

    #[tokio::test]
    async fn list_users_and_states_orders_by_updated_at_desc() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let a = FileKind::User {
            user_id: "1".into(),
        };
        let b = FileKind::User {
            user_id: "2".into(),
        };
        store
            .write(&a, "first", UserIdentity::of("a", "A"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        store
            .write(&b, "second", UserIdentity::of("b", "B"))
            .await
            .unwrap();
        let list = store.list_users().await.unwrap();
        assert_eq!(list[0].kind, b);
        assert_eq!(list[1].kind, a);
    }

    #[tokio::test]
    async fn write_scratch_sets_created_by_on_create_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::Scratch {
            slug: "quiz".into(),
        };
        store
            .write_scratch(&kind, "score: 1", Some("12345"))
            .await
            .unwrap();
        let one = store.read_kind(&kind).await.unwrap();
        assert_eq!(one.frontmatter.created_by.as_deref(), Some("12345"));
        store
            .write_scratch(&kind, "score: 2", Some("99999"))
            .await
            .unwrap();
        let two = store.read_kind(&kind).await.unwrap();
        assert_eq!(
            two.frontmatter.created_by.as_deref(),
            Some("12345"),
            "created_by stays after overwrite"
        );
    }

    #[tokio::test]
    async fn current_mtime_returns_zero_for_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let mt = store
            .current_mtime(&FileKind::User {
                user_id: "404".into(),
            })
            .await
            .unwrap();
        assert_eq!(mt, 0, "missing file must report mtime 0");
        let mt = store
            .current_mtime(&FileKind::Scratch {
                slug: "missing".into(),
            })
            .await
            .unwrap();
        assert_eq!(mt, 0);
    }

    #[tokio::test]
    async fn delete_scratch_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::Scratch {
            slug: "quiz".into(),
        };
        store.write_scratch(&kind, "x", Some("1")).await.unwrap();
        store.delete_scratch("quiz").await.unwrap();
        assert!(!dir.path().join("memories/scratch/quiz.md").exists());
    }

    #[tokio::test]
    async fn forget_user_removes_only_that_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        for id in ["111", "222"] {
            let kind = FileKind::User { user_id: id.into() };
            store
                .write(&kind, "mentions 111", UserIdentity::default())
                .await
                .unwrap();
        }
        store
            .write(&FileKind::Lore, "111 did a bit", UserIdentity::default())
            .await
            .unwrap();

        assert!(store.forget_user("111").await.unwrap());
        let users = dir.path().join("memories/users");
        assert!(!users.join("111.md").exists());
        assert!(users.join("222.md").exists());
        let lore = store.read_kind(&FileKind::Lore).await.unwrap();
        assert!(lore.body.contains("111 did a bit"));

        assert!(!store.forget_user("111").await.unwrap(), "already gone");
        assert!(store.forget_user("../LORE").await.is_err());
        assert!(dir.path().join("memories/LORE.md").exists());
    }

    #[tokio::test]
    async fn write_with_guard_detects_concurrent_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        // Bump SOUL.md so we have a known mtime to test against.
        store
            .write(&FileKind::Soul, "first", UserIdentity::default())
            .await
            .unwrap();
        let mt1 = store.current_mtime(&FileKind::Soul).await.unwrap();

        // Stale token → Conflict.
        let outcome = store
            .write_with_guard(
                FileKind::Soul,
                "loser",
                Some(0),
                FrontmatterOverride::default(),
            )
            .await
            .unwrap();
        match outcome {
            WriteOutcome::Conflict {
                current_body,
                current_mtime,
            } => {
                assert!(current_body.contains("first"));
                assert_eq!(current_mtime, mt1);
            }
            WriteOutcome::Written { .. } => panic!("expected Conflict for stale mtime"),
        }
        // The file must NOT have been overwritten.
        let mf = store.read_kind(&FileKind::Soul).await.unwrap();
        assert!(mf.body.contains("first"));

        // Fresh token → Written.
        let outcome = store
            .write_with_guard(
                FileKind::Soul,
                "winner",
                Some(mt1),
                FrontmatterOverride::default(),
            )
            .await
            .unwrap();
        let new_mt = match outcome {
            WriteOutcome::Written { new_mtime } => new_mtime,
            WriteOutcome::Conflict { .. } => panic!("expected Written for fresh mtime"),
        };
        assert!(new_mt >= mt1);
        let mf = store.read_kind(&FileKind::Soul).await.unwrap();
        assert!(mf.body.contains("winner"));
    }

    #[tokio::test]
    async fn write_with_guard_unconditional_when_expected_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        store
            .write(&FileKind::Lore, "before", UserIdentity::default())
            .await
            .unwrap();
        let outcome = store
            .write_with_guard(
                FileKind::Lore,
                "after",
                None,
                FrontmatterOverride::default(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, WriteOutcome::Written { .. }));
        let mf = store.read_kind(&FileKind::Lore).await.unwrap();
        assert!(mf.body.contains("after"));
    }

    #[tokio::test]
    async fn user_file_written_without_identity_keeps_blank_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::User {
            user_id: "7".into(),
        };
        store
            .write(&kind, "body", UserIdentity::default())
            .await
            .unwrap();
        let raw = tokio::fs::read_to_string(store.memories_dir().join("users/7.md"))
            .await
            .unwrap();
        assert!(raw.contains("\nusername:\n"), "{raw}");
        assert!(raw.contains("\ndisplay_name:\n"), "{raw}");
        // Blank round-trips as unknown, so a later write can still fill it.
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.frontmatter.username, None);
        store
            .write(&kind, "body", UserIdentity::of("late", "Late"))
            .await
            .unwrap();
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.frontmatter.username.as_deref(), Some("late"));
    }

    #[tokio::test]
    async fn links_survive_a_chat_turn_write_and_clear_only_on_override() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::User {
            user_id: "42".into(),
        };
        store
            .write(&kind, "body", UserIdentity::of("alice", "Alice"))
            .await
            .unwrap();
        let mt = store.current_mtime(&kind).await.unwrap();
        let links = BTreeMap::from([("steam".to_string(), "https://s.example/alice".to_string())]);
        store
            .write_with_guard(
                kind.clone(),
                "body",
                Some(mt),
                FrontmatterOverride {
                    links: Some(links.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // A chat-turn write passes no links at all; they must not vanish.
        store
            .write(&kind, "new body", UserIdentity::of("alice", "Alice"))
            .await
            .unwrap();
        assert_eq!(
            store.read_kind(&kind).await.unwrap().frontmatter.links,
            links
        );

        // Only an explicit empty override clears them.
        let mt = store.current_mtime(&kind).await.unwrap();
        store
            .write_with_guard(
                kind.clone(),
                "body",
                Some(mt),
                FrontmatterOverride {
                    links: Some(BTreeMap::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            store
                .read_kind(&kind)
                .await
                .unwrap()
                .frontmatter
                .links
                .is_empty()
        );
    }

    #[tokio::test]
    async fn write_with_guard_override_replaces_user_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::User {
            user_id: "42".into(),
        };
        store
            .write(&kind, "body", UserIdentity::of("oldlogin", "OldDisplay"))
            .await
            .unwrap();
        let mt = store.current_mtime(&kind).await.unwrap();
        let fm = FrontmatterOverride {
            username: Some("newlogin".into()),
            display_name: Some("NewDisplay".into()),
            created_by: None,
            links: None,
        };
        let outcome = store
            .write_with_guard(kind.clone(), "body2", Some(mt), fm)
            .await
            .unwrap();
        assert!(matches!(outcome, WriteOutcome::Written { .. }));
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.frontmatter.username.as_deref(), Some("newlogin"));
        assert_eq!(mf.frontmatter.display_name.as_deref(), Some("NewDisplay"));
    }

    #[tokio::test]
    async fn write_with_guard_override_replaces_state_created_by() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::Scratch {
            slug: "quiz".into(),
        };
        store
            .write_scratch(&kind, "x", Some("12345"))
            .await
            .unwrap();
        let mt = store.current_mtime(&kind).await.unwrap();
        let fm = FrontmatterOverride {
            created_by: Some("99999".into()),
            ..FrontmatterOverride::default()
        };
        store
            .write_with_guard(kind.clone(), "y", Some(mt), fm)
            .await
            .unwrap();
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.frontmatter.created_by.as_deref(), Some("99999"));
    }

    #[tokio::test]
    async fn write_with_guard_empty_override_preserves_state_created_by() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::Scratch {
            slug: "notes".into(),
        };
        store
            .write_scratch(&kind, "x", Some("12345"))
            .await
            .unwrap();
        let mt = store.current_mtime(&kind).await.unwrap();
        let fm = FrontmatterOverride {
            created_by: Some(String::new()),
            ..FrontmatterOverride::default()
        };
        store
            .write_with_guard(kind.clone(), "y", Some(mt), fm)
            .await
            .unwrap();
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.frontmatter.created_by.as_deref(), Some("12345"));
    }

    /// Store-level gate for every slug-taking scratch op; the full slug grammar
    /// is covered in `sanitize`. Reserved and invalid map to distinct errors.
    #[tokio::test]
    async fn scratch_slug_gate_rejects_reserved_and_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let err = store
            .write_scratch(
                &FileKind::Scratch {
                    slug: "system".into(),
                },
                "x",
                Some("1"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::ReservedSlug), "got {err:?}");

        let too_long = "x".repeat(65);
        let cases = [
            too_long.as_str(),
            "",
            "with/slash",
            "..",
            "with..dots",
            "Foo",
            "has_underscore",
            "av-depot-2026-05-08",
        ];
        for slug in cases {
            let err = store
                .write_scratch(&FileKind::Scratch { slug: slug.into() }, "x", Some("1"))
                .await
                .unwrap_err();
            assert!(
                matches!(err, WriteError::InvalidSlug),
                "slug `{slug}` must be rejected, got {err:?}"
            );
        }

        let err = store.delete_scratch("legacy-2026-05-08").await.unwrap_err();
        assert!(matches!(err, WriteError::InvalidSlug));
    }

    /// Verify that `MemoryStore::caps()` reflects settings changes published
    /// after construction — i.e., the store holds a live handle, not a snapshot.
    #[tokio::test]
    async fn memory_store_caps_rebind_live() {
        let dir = tempfile::tempdir().unwrap();
        let handle = test_handle();
        let store = MemoryStore::open(dir.path(), handle.clone()).await.unwrap();
        // Compiled default is 4096 bytes.
        assert_eq!(store.caps().soul_bytes, 4096);

        let mut new_settings = Settings::compiled_defaults();
        new_settings.ai.memory.soul_bytes = 8192;
        handle.store(Arc::new(new_settings));

        assert_eq!(
            store.caps().soul_bytes,
            8192,
            "store must reflect live settings update"
        );
    }
}
