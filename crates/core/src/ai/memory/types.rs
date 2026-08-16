//! Public types shared across the v2 memory module.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};

/// Speaker / actor role. `Dreamer` is the ritual LLM, never a Twitch user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Role {
    Regular = 0,
    Moderator = 1,
    Broadcaster = 2,
    Dreamer = 3,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Regular => "regular",
            Role::Moderator => "moderator",
            Role::Broadcaster => "broadcaster",
            Role::Dreamer => "dreamer",
        }
    }
}

/// Identifies one file under `memories/`.
///
/// ADR-0003: `Soul`/`Lore`/`User` are Memory; `Scratch` is not. The enum (and
/// `MemoryStore`) deliberately span both — the glossary governs domain
/// language, not type names, and renaming the container would migrate every
/// prod file for no domain gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    Soul,
    Lore,
    User { user_id: String },
    Scratch { slug: String },
}

impl FileKind {
    pub fn relative_path(&self) -> PathBuf {
        match self {
            FileKind::Soul => PathBuf::from("SOUL.md"),
            FileKind::Lore => PathBuf::from("LORE.md"),
            FileKind::User { user_id } => PathBuf::from(format!("users/{user_id}.md")),
            FileKind::Scratch { slug } => PathBuf::from(format!("scratch/{slug}.md")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub updated_at: DateTime<Utc>,
    /// Twitch login (lowercase username). Set on user files; absent on
    /// SOUL/LORE/scratch.
    pub username: Option<String>,
    /// Twitch display name (cased). Set on user files; absent on
    /// SOUL/LORE/scratch.
    pub display_name: Option<String>,
    pub created_by: Option<String>,
    /// Social/profile links, `platform -> url` (steam, github, ...). Free-form
    /// keys so a new platform needs no code change. User files only; empty map
    /// means none. Written from the dashboard, read-only to the model.
    pub links: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub kind: FileKind,
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// Per-file byte caps and the global scratch-file count cap.
///
/// Byte caps apply to the *full emitted file* (frontmatter + body), so the
/// on-disk size never exceeds the configured limit. The cap is enforced
/// inside the per-path mutex in `MemoryStore::write` / `write_scratch`, so
/// concurrent writes to the same file cannot smuggle an over-cap body past
/// the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub soul_bytes: usize,
    pub lore_bytes: usize,
    pub user_bytes: usize,
    pub scratch_bytes: usize,
    /// Max number of `scratch/<slug>.md` files that may exist at once.
    /// Enforced inside `MemoryStore::write_scratch` under a global
    /// scratch-create lock so two concurrent new-slug writes can't both pass
    /// the cap check.
    pub max_scratch_files: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            soul_bytes: 4096,
            lore_bytes: 12_288,
            user_bytes: 4096,
            scratch_bytes: 2048,
            max_scratch_files: 16,
        }
    }
}

impl Caps {
    pub fn limit_for(&self, kind: &FileKind) -> usize {
        match kind {
            FileKind::Soul => self.soul_bytes,
            FileKind::Lore => self.lore_bytes,
            FileKind::User { .. } => self.user_bytes,
            FileKind::Scratch { .. } => self.scratch_bytes,
        }
    }

    /// Build a `Caps` snapshot from the live settings. Placed here so every
    /// call site (store, tests) can derive caps the same way without depending
    /// on `ai::command`.
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        let m = &s.ai.memory;
        Self {
            soul_bytes: m.soul_bytes,
            lore_bytes: m.lore_bytes,
            user_bytes: m.user_bytes,
            scratch_bytes: m.scratch_bytes,
            max_scratch_files: m.max_scratch_files,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_orderable_for_perm_table() {
        assert_eq!(Role::Regular as u8 + 1, Role::Moderator as u8);
        assert_eq!(Role::Moderator as u8 + 1, Role::Broadcaster as u8);
        assert_eq!(Role::Broadcaster as u8 + 1, Role::Dreamer as u8);
    }

    #[test]
    fn caps_default_matches_spec() {
        let c = Caps::default();
        assert_eq!(c.soul_bytes, 4096);
        assert_eq!(c.lore_bytes, 12_288);
        assert_eq!(c.user_bytes, 4096);
        assert_eq!(c.scratch_bytes, 2048);
        assert_eq!(c.max_scratch_files, 16);
    }

    #[test]
    fn file_kind_relative_path_round_trips() {
        assert_eq!(FileKind::Soul.relative_path().to_str().unwrap(), "SOUL.md");
        assert_eq!(FileKind::Lore.relative_path().to_str().unwrap(), "LORE.md");
        assert_eq!(
            FileKind::User {
                user_id: "12345".into()
            }
            .relative_path()
            .to_str()
            .unwrap(),
            "users/12345.md"
        );
        assert_eq!(
            FileKind::Scratch {
                slug: "quiz".into()
            }
            .relative_path()
            .to_str()
            .unwrap(),
            "scratch/quiz.md"
        );
    }
}
