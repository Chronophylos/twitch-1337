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

    pub async fn write(
        &self,
        kind: &FileKind,
        body: &str,
        display_name: Option<&str>,
    ) -> Result<(), WriteError> {
        let limit = self.inner.caps.limit_for(kind);
        let now = Utc::now();

        let display_name = if matches!(kind, FileKind::User { .. }) {
            display_name
                .map(normalize_display_name)
                .filter(|s| !s.is_empty())
        } else {
            None
        };

        if body.len() > limit {
            return Err(WriteError::Full);
        }

        let fm = Frontmatter {
            updated_at: now,
            display_name,
            created_by: None,
        };
        let raw = frontmatter::emit(&fm, body);

        let rel = kind.relative_path();
        let abs = self.inner.memories_dir.join(&rel);
        let lock = self.lock_for(&rel).await;
        let _g = lock.lock().await;

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

    pub async fn write_state(
        &self,
        kind: &FileKind,
        body: &str,
        creator_user_id: Option<&str>,
    ) -> Result<(), WriteError> {
        let FileKind::State { slug } = kind else {
            return Err(WriteError::Io(eyre!(
                "write_state called on non-state kind"
            )));
        };
        let limit = self.inner.caps.state_bytes;
        let now = Utc::now();

        let rel = kind.relative_path();
        let abs = self.inner.memories_dir.join(&rel);
        let lock = self.lock_for(&rel).await;
        let _g = lock.lock().await;

        // Preserve existing created_by; only set on first write.
        let prior_created_by = match tokio::fs::read_to_string(&abs).await {
            Ok(raw) => frontmatter::parse(&raw)
                .ok()
                .and_then(|(fm, _)| fm.created_by),
            Err(_) => None,
        };
        let created_by = prior_created_by
            .or_else(|| creator_user_id.map(std::string::ToString::to_string));

        if body.len() > limit {
            return Err(WriteError::Full);
        }

        let fm = Frontmatter {
            updated_at: now,
            display_name: None,
            created_by,
        };
        let raw = frontmatter::emit(&fm, body);
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| WriteError::Io(eyre!(e)))?;
        }
        atomic_write_bytes_async(raw.as_bytes(), &abs)
            .await
            .map_err(|e| WriteError::Io(eyre!(e)))?;
        let _ = slug; // slug already encoded in rel path
        Ok(())
    }

    pub async fn delete_state(&self, slug: &str) -> Result<()> {
        let rel = PathBuf::from(format!("state/{slug}.md"));
        let abs = self.inner.memories_dir.join(&rel);
        let lock = self.lock_for(&rel).await;
        let _g = lock.lock().await;
        match tokio::fs::remove_file(&abs).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(eyre!(e)),
        }
    }

    pub async fn list_users(&self) -> Result<Vec<MemoryFile>> {
        self.list_in_subdir("users", |stem| FileKind::User {
            user_id: stem.into(),
        })
        .await
    }

    pub async fn list_state(&self) -> Result<Vec<MemoryFile>> {
        self.list_in_subdir("state", |stem| FileKind::State { slug: stem.into() })
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
        out.sort_by(|a, b| b.frontmatter.updated_at.cmp(&a.frontmatter.updated_at));
        Ok(out)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("file_full")]
    Full,
    #[error("io: {0}")]
    Io(#[from] eyre::Report),
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

    #[tokio::test]
    async fn write_user_file_persists_and_caps_apply() {
        let dir = tempfile::tempdir().unwrap();
        let caps = Caps {
            user_bytes: 32, // tiny on purpose
            ..Caps::default()
        };
        let store = MemoryStore::open(dir.path(), caps).await.unwrap();
        let kind = FileKind::User {
            user_id: "12".into(),
        };

        store
            .write(&kind, "small body", Some("alice"))
            .await
            .unwrap();
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.body.trim(), "small body");
        assert_eq!(mf.frontmatter.display_name.as_deref(), Some("alice"));

        let huge = "x".repeat(4096);
        let err = store.write(&kind, &huge, Some("alice")).await.unwrap_err();
        assert_eq!(err.to_string(), "file_full");
    }

    #[tokio::test]
    async fn display_name_is_normalised_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let kind = FileKind::User {
            user_id: "99".into(),
        };
        let dirty = "ali\u{200B}ce\nx";
        store.write(&kind, "body", Some(dirty)).await.unwrap();
        let mf = store.read_kind(&kind).await.unwrap();
        assert_eq!(mf.frontmatter.display_name.as_deref(), Some("alicex"));
    }

    #[tokio::test]
    async fn list_users_and_states_orders_by_updated_at_desc() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let a = FileKind::User {
            user_id: "1".into(),
        };
        let b = FileKind::User {
            user_id: "2".into(),
        };
        store.write(&a, "first", Some("a")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        store.write(&b, "second", Some("b")).await.unwrap();
        let list = store.list_users().await.unwrap();
        assert_eq!(list[0].kind, b);
        assert_eq!(list[1].kind, a);
    }

    #[tokio::test]
    async fn write_state_sets_created_by_on_create_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let kind = FileKind::State {
            slug: "quiz".into(),
        };
        store
            .write_state(&kind, "score: 1", Some("12345"))
            .await
            .unwrap();
        let one = store.read_kind(&kind).await.unwrap();
        assert_eq!(one.frontmatter.created_by.as_deref(), Some("12345"));
        store
            .write_state(&kind, "score: 2", Some("99999"))
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
    async fn delete_state_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let kind = FileKind::State {
            slug: "quiz".into(),
        };
        store.write_state(&kind, "x", Some("1")).await.unwrap();
        store.delete_state("quiz").await.unwrap();
        assert!(!dir.path().join("memories/state/quiz.md").exists());
    }
}
