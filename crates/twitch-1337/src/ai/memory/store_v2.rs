//! Filesystem layer for v2 memory: read/write/list under per-path mutex,
//! atomic tmp+rename, byte caps, soul + prompt seeding, v1 disposal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use eyre::{Result, WrapErr as _, eyre};
use tokio::sync::Mutex;
use tracing::info;

use crate::ai::memory::frontmatter;
use crate::ai::memory::sanitize::normalize_display_name;
use crate::ai::memory::types::{Caps, FileKind, Frontmatter, MemoryFile};
use crate::util::persist::atomic_write_bytes_async;

const SOUL_SEED: &str = include_str!("../../../data/prompts/seed_soul.md");
const PROMPT_SYSTEM: &str = include_str!("../../../data/prompts/system.md");
const PROMPT_INSTRUCTIONS: &str = include_str!("../../../data/prompts/ai_instructions.md");
const PROMPT_DREAMER: &str = include_str!("../../../data/prompts/dreamer.md");

#[derive(Clone)]
pub struct MemoryStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    root: PathBuf,         // $DATA_DIR
    memories_dir: PathBuf, // $DATA_DIR/memories
    prompts_dir: PathBuf,  // $DATA_DIR/prompts
    caps: Caps,
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl MemoryStore {
    pub async fn open(data_dir: &Path, caps: Caps) -> Result<Self> {
        let memories_dir = data_dir.join("memories");
        let prompts_dir = data_dir.join("prompts");
        for p in [
            &memories_dir,
            &memories_dir.join("users"),
            &memories_dir.join("state"),
            &memories_dir.join("transcripts"),
            &prompts_dir,
        ] {
            tokio::fs::create_dir_all(p)
                .await
                .wrap_err_with(|| format!("create_dir_all {}", p.display()))?;
        }

        // v1 disposal.
        let v1 = data_dir.join("ai_memory.ron");
        if tokio::fs::try_exists(&v1).await.unwrap_or(false) {
            let ts = Utc::now().timestamp();
            let dest = data_dir.join(format!("ai_memory.ron.discarded-{ts}"));
            tokio::fs::rename(&v1, &dest).await.ok();
            info!(target = %dest.display(), "Renamed v1 ai_memory.ron — v2 starts fresh");
        }

        // SOUL.md seed.
        let soul_path = memories_dir.join("SOUL.md");
        if !tokio::fs::try_exists(&soul_path).await.unwrap_or(false) {
            let fm = Frontmatter {
                updated_at: Utc::now(),
                display_name: None,
                created_by: None,
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
                updated_at: Utc::now(),
                display_name: None,
                created_by: None,
            };
            atomic_write_bytes_async(frontmatter::emit(&fm, "").as_bytes(), &lore_path)
                .await
                .wrap_err("seed LORE.md")?;
        }
        // Prompt files: write defaults only when missing.
        for (name, default) in [
            ("system.md", PROMPT_SYSTEM),
            ("ai_instructions.md", PROMPT_INSTRUCTIONS),
            ("dreamer.md", PROMPT_DREAMER),
        ] {
            let p = prompts_dir.join(name);
            if !tokio::fs::try_exists(&p).await.unwrap_or(false) {
                atomic_write_bytes_async(default.as_bytes(), &p)
                    .await
                    .wrap_err_with(|| format!("seed prompt {name}"))?;
            }
        }

        Ok(Self {
            inner: Arc::new(StoreInner {
                root: data_dir.to_path_buf(),
                memories_dir,
                prompts_dir,
                caps,
                locks: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn caps(&self) -> Caps {
        self.inner.caps
    }

    pub fn memories_dir(&self) -> &Path {
        &self.inner.memories_dir
    }

    pub fn prompts_dir(&self) -> &Path {
        &self.inner.prompts_dir
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

    /// Read a single file by `FileKind`. Missing → empty body, default frontmatter.
    pub async fn read_kind(&self, kind: &FileKind) -> Result<MemoryFile> {
        let rel = kind.relative_path();
        let abs = self.inner.memories_dir.join(&rel);
        match tokio::fs::read_to_string(&abs).await {
            Ok(raw) => {
                let (fm, body) =
                    frontmatter::parse(&raw).map_err(|e| eyre!("parse {}: {e}", rel.display()))?;
                Ok(MemoryFile {
                    kind: kind.clone(),
                    frontmatter: fm,
                    body,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MemoryFile {
                kind: kind.clone(),
                frontmatter: Frontmatter {
                    updated_at: Utc::now(),
                    display_name: None,
                    created_by: None,
                },
                body: String::new(),
            }),
            Err(e) => Err(eyre!(e)),
        }
    }

    #[allow(dead_code)] // used by T7+ until full wiring lands
    pub fn _silence_unused(&self) {
        let _ = (&self.inner.root, normalize_display_name(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::memory::types::Caps;

    #[tokio::test]
    async fn open_creates_tree_and_seeds_soul() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        assert!(dir.path().join("memories/SOUL.md").exists());
        assert!(dir.path().join("memories/users").is_dir());
        assert!(dir.path().join("memories/state").is_dir());
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
        MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
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
    async fn open_seeds_prompts_on_first_run_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let p = dir.path().join("prompts/system.md");
        assert!(p.exists());
        tokio::fs::write(&p, b"USER EDITED").await.unwrap();
        // Reopen: edited file must be preserved.
        let _ = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert_eq!(s, "USER EDITED");
        let _ = store; // suppress unused
    }
}
