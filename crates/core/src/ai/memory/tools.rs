//! Tool args + definitions for the v2 chat-turn and dreamer loops.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use llm::{ToolCall, ToolDefinition, ToolExecutor, ToolResultMessage};

use crate::ai::memory::sanitize::{
    PathError, SlugError, WritePath, check_body, has_trailing_iso_date, parse_slug,
    parse_write_path, write_path_to_kind,
};
use crate::ai::memory::store::{MemoryStore, WriteError};
use crate::ai::memory::types::{FileKind, Role};

// ---------------------------------------------------------------------------
// Arg structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    pub path: String,
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteScratchArgs {
    pub slug: String,
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteScratchArgs {
    pub slug: String,
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

pub fn chat_turn_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::derived::<WriteFileArgs>(
            "write_file",
            "Overwrite a memory file (SOUL.md, LORE.md, or users/<id>.md). Body is the new full prose body; frontmatter is store-managed. Permission-gated by speaker role.",
        ),
        ToolDefinition::derived::<WriteScratchArgs>(
            "write_scratch",
            "Create or overwrite a scratch file at scratch/<slug>.md. slug is lowercase a–z 0–9 dashes, ≤64 chars.",
        ),
        ToolDefinition::derived::<DeleteScratchArgs>(
            "delete_scratch",
            "Remove a scratch file. Regulars may only delete scratch files they created.",
        ),
    ]
}

pub fn dreamer_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::derived::<WriteFileArgs>(
            "write_file",
            "Overwrite SOUL.md / LORE.md / users/<id>.md.",
        ),
        ToolDefinition::derived::<WriteScratchArgs>(
            "write_scratch",
            "Overwrite scratch/<slug>.md.",
        ),
        ToolDefinition::derived::<DeleteScratchArgs>(
            "delete_scratch",
            "Remove a stale scratch file.",
        ),
    ]
}

// ---------------------------------------------------------------------------
// ChatTurnExecutor
// ---------------------------------------------------------------------------

pub struct ChatTurnExecutorOpts {
    pub store: MemoryStore,
    pub speaker_user_id: String,
    pub speaker_login: String,
    pub speaker_display_name: String,
    pub speaker_role: Role,
    /// Maximum write-class tool calls (write_file, write_scratch, delete_scratch)
    /// per turn.
    pub max_writes_per_turn: usize,
}

pub struct ChatTurnExecutor {
    opts: ChatTurnExecutorOpts,
    write_count: AtomicUsize,
}

impl ChatTurnExecutor {
    pub fn new(opts: ChatTurnExecutorOpts) -> Self {
        Self {
            opts,
            write_count: AtomicUsize::new(0),
        }
    }

    fn role(&self) -> Role {
        self.opts.speaker_role
    }

    /// Returns `None` if the speaker may write `path`, or `Some(reason)`
    /// naming which rule blocked it (used as the `permission_denied:` clause).
    fn deny_reason_for_write_path(&self, path: &WritePath) -> Option<&'static str> {
        match (path, self.role()) {
            (WritePath::Soul, Role::Dreamer) => None,
            (WritePath::Soul, _) => Some("SOUL.md is rewritten only by the daily dreamer ritual"),
            (WritePath::Lore, Role::Regular) => {
                Some("editing LORE.md requires moderator or higher")
            }
            (WritePath::Lore, _) => None,
            (WritePath::User { user_id }, Role::Regular) => {
                if user_id == &self.opts.speaker_user_id {
                    None
                } else {
                    Some("regulars may only edit their own user memory file")
                }
            }
            (WritePath::User { .. }, _) => None,
        }
    }

    /// Attempt to consume one write from the per-turn quota.
    /// Returns `true` if the write is allowed, `false` when the quota is
    /// already exhausted.
    fn try_consume_write_quota(&self) -> bool {
        // fetch_add returns the *previous* value before the increment.
        let used = self.write_count.fetch_add(1, Ordering::SeqCst);
        if used >= self.opts.max_writes_per_turn {
            // Undo the spurious increment so the counter doesn't overflow on
            // repeated exhausted calls.
            self.write_count
                .store(self.opts.max_writes_per_turn, Ordering::SeqCst);
            false
        } else {
            true
        }
    }

    async fn handle_write_file(&self, call: &ToolCall) -> String {
        let args: WriteFileArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments: could not parse the tool arguments".into(),
        };
        let path = match parse_write_path(&args.path) {
            Ok(p) => p,
            Err(PathError) => {
                return "invalid_path: path must be SOUL.md, LORE.md, or users/<numeric-id>.md"
                    .into();
            }
        };
        if let Some(reason) = self.deny_reason_for_write_path(&path) {
            return format!("permission_denied: {reason}");
        }
        if check_body(&args.body).is_err() {
            return "invalid_body: body contains a reserved marker (frontmatter fence, path header, or carriage return)".into();
        }
        if !self.try_consume_write_quota() {
            return "write_quota_exhausted: the write limit for this turn is used up".into();
        }

        let kind = write_path_to_kind(path);
        // Pass identity only for the speaker's own user file. For other user
        // files, `None` lets the store preserve whatever's already on disk.
        let (login, display) = if matches!(&kind, FileKind::User { user_id } if user_id == &self.opts.speaker_user_id)
        {
            (
                Some(self.opts.speaker_login.as_str()),
                Some(self.opts.speaker_display_name.as_str()),
            )
        } else {
            (None, None)
        };
        match self
            .opts
            .store
            .write(&kind, &args.body, login, display)
            .await
        {
            Ok(()) => "ok".into(),
            Err(WriteError::Full) => "file_full: body exceeds the byte cap for this file".into(),
            Err(WriteError::ScratchFull) => {
                "scratch_full: the maximum number of scratch files already exists".into()
            } // unreachable for write()
            Err(WriteError::InvalidSlug) => {
                "invalid_slug: slug must be lowercase a-z, 0-9, and dashes, up to 64 chars".into()
            } // unreachable for non-scratch write
            Err(WriteError::Io(e)) => format!("io_error: {e}"),
        }
    }

    async fn handle_write_scratch(&self, call: &ToolCall) -> String {
        let args: WriteScratchArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments: could not parse the tool arguments".into(),
        };
        let slug = match parse_slug(&args.slug) {
            Ok(s) => s,
            Err(SlugError::Invalid) => {
                return "invalid_slug: slug must be lowercase a-z, 0-9, and dashes, up to 64 chars"
                    .into();
            }
            Err(SlugError::Reserved) => return "reserved_slug: that slug name is reserved".into(),
        };
        if has_trailing_iso_date(&slug) {
            return "dated_slug: scratch slugs may not end in a -YYYY-MM-DD date suffix".into();
        }
        if check_body(&args.body).is_err() {
            return "invalid_body: body contains a reserved marker (frontmatter fence, path header, or carriage return)".into();
        }
        if !self.try_consume_write_quota() {
            return "write_quota_exhausted: the write limit for this turn is used up".into();
        }

        match self
            .opts
            .store
            .write_scratch(
                &FileKind::Scratch { slug },
                &args.body,
                Some(self.opts.speaker_user_id.as_str()),
            )
            .await
        {
            Ok(()) => "ok".into(),
            Err(WriteError::Full) => "file_full: body exceeds the byte cap for this file".into(),
            Err(WriteError::ScratchFull) => {
                "scratch_full: the maximum number of scratch files already exists".into()
            }
            Err(WriteError::InvalidSlug) => {
                "invalid_slug: slug must be lowercase a-z, 0-9, and dashes, up to 64 chars".into()
            }
            Err(WriteError::Io(e)) => format!("io_error: {e}"),
        }
    }

    async fn handle_delete_scratch(&self, call: &ToolCall) -> String {
        let args: DeleteScratchArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments: could not parse the tool arguments".into(),
        };
        let slug = match parse_slug(&args.slug) {
            Ok(s) => s,
            Err(SlugError::Invalid) => {
                return "invalid_slug: slug must be lowercase a-z, 0-9, and dashes, up to 64 chars"
                    .into();
            }
            Err(SlugError::Reserved) => return "reserved_slug: that slug name is reserved".into(),
        };

        // Check ownership before consuming quota.
        let owner = self
            .opts
            .store
            .read_kind(&FileKind::Scratch { slug: slug.clone() })
            .await
            .ok()
            .and_then(|f| f.frontmatter.created_by);
        let allowed = match self.role() {
            Role::Regular => owner.as_deref() == Some(self.opts.speaker_user_id.as_str()),
            _ => true,
        };
        if !allowed {
            return "permission_denied: regulars may only delete scratch files they created".into();
        }

        if !self.try_consume_write_quota() {
            return "write_quota_exhausted: the write limit for this turn is used up".into();
        }

        match self.opts.store.delete_scratch(&slug).await {
            Ok(()) => "ok".into(),
            Err(e) => format!("io_error: {e}"),
        }
    }
}

#[async_trait]
impl ToolExecutor for ChatTurnExecutor {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        let content = match call.name.as_str() {
            "write_file" => self.handle_write_file(call).await,
            "write_scratch" => self.handle_write_scratch(call).await,
            "delete_scratch" => self.handle_delete_scratch(call).await,
            _ => "unknown_tool: no memory tool with that name".to_string(),
        };
        ToolResultMessage::for_call(call, content)
    }
}

// ---------------------------------------------------------------------------
// DreamerExecutor
// ---------------------------------------------------------------------------

pub struct DreamerExecutorOpts {
    pub store: MemoryStore,
    pub max_writes_per_turn: usize,
}

pub struct DreamerExecutor {
    inner: ChatTurnExecutor,
}

impl DreamerExecutor {
    pub fn new(opts: DreamerExecutorOpts) -> Self {
        Self {
            inner: ChatTurnExecutor::new(ChatTurnExecutorOpts {
                store: opts.store,
                speaker_user_id: "dreamer".into(),
                speaker_login: String::new(),
                speaker_display_name: String::new(),
                speaker_role: Role::Dreamer,
                max_writes_per_turn: opts.max_writes_per_turn,
            }),
        }
    }
}

#[async_trait]
impl ToolExecutor for DreamerExecutor {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        self.inner.execute(call).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod schema_tests {
    use super::*;

    #[test]
    fn chat_turn_tools_has_three_named_tools() {
        let names: Vec<_> = chat_turn_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["write_file", "write_scratch", "delete_scratch"]);
    }

    #[test]
    fn dreamer_tools_matches_chat_turn_tools() {
        let names: Vec<_> = dreamer_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["write_file", "write_scratch", "delete_scratch"]);
    }

    #[test]
    fn write_file_args_round_trip() {
        let v = serde_json::json!({"path": "users/12.md", "body": "hi"});
        let parsed: WriteFileArgs = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.path, "users/12.md");
        assert_eq!(parsed.body, "hi");
    }
}

#[cfg(test)]
mod exec_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;

    use super::*;
    use crate::ai::memory::store::MemoryStore;
    use crate::ai::memory::types::Caps;
    use crate::settings::Settings;

    fn test_handle() -> crate::settings::SettingsHandle {
        Arc::new(ArcSwap::from_pointee(Settings::compiled_defaults()))
    }

    fn handle_with_caps(caps: Caps) -> crate::settings::SettingsHandle {
        let mut s = Settings::compiled_defaults();
        s.ai.memory.soul_bytes = caps.soul_bytes;
        s.ai.memory.lore_bytes = caps.lore_bytes;
        s.ai.memory.user_bytes = caps.user_bytes;
        s.ai.memory.scratch_bytes = caps.scratch_bytes;
        s.ai.memory.max_scratch_files = caps.max_scratch_files;
        Arc::new(ArcSwap::from_pointee(s))
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    async fn make_executor(role: Role, store: MemoryStore, max_writes: usize) -> ChatTurnExecutor {
        ChatTurnExecutor::new(ChatTurnExecutorOpts {
            store,
            speaker_user_id: "12345".into(),
            speaker_login: "alice".into(),
            speaker_display_name: "Alice".into(),
            speaker_role: role,
            max_writes_per_turn: max_writes,
        })
    }

    #[tokio::test]
    async fn regular_writes_own_user_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/12345.md", "body": "hi"}),
            ))
            .await;
        assert_eq!(r.content, "ok");
    }

    #[tokio::test]
    async fn regular_cannot_write_other_user_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/99.md", "body": "hi"}),
            ))
            .await;
        assert!(
            r.content.starts_with("permission_denied"),
            "got {}",
            r.content
        );
    }

    #[tokio::test]
    async fn permission_denials_have_distinct_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        let soul = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "SOUL.md", "body": "x"}),
            ))
            .await
            .content;
        let lore = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "LORE.md", "body": "x"}),
            ))
            .await
            .content;
        let other = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/99.md", "body": "x"}),
            ))
            .await
            .content;
        for r in [&soul, &lore, &other] {
            assert!(r.starts_with("permission_denied: "), "missing prefix: {r}");
        }
        // Each rule yields its own reason.
        assert_ne!(soul, lore);
        assert_ne!(lore, other);
        assert_ne!(soul, other);
    }

    #[tokio::test]
    async fn regular_cannot_write_lore_or_soul() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        for path in ["LORE.md", "SOUL.md"] {
            let r = exec
                .execute(&call(
                    "write_file",
                    serde_json::json!({"path": path, "body": "x"}),
                ))
                .await;
            assert!(
                r.content.starts_with("permission_denied"),
                "regular wrote {path}: {}",
                r.content
            );
        }
    }

    #[tokio::test]
    async fn moderator_can_write_lore_but_not_soul() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Moderator, store.clone(), 8).await;
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "LORE.md", "body": "x"}),
            ))
            .await;
        assert_eq!(r.content, "ok");
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "SOUL.md", "body": "x"}),
            ))
            .await;
        assert!(
            r.content.starts_with("permission_denied"),
            "got {}",
            r.content
        );
    }

    #[tokio::test]
    async fn invalid_path_and_invalid_slug_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "../etc/passwd", "body": "x"}),
            ))
            .await;
        assert!(r.content.starts_with("invalid_path"), "got {}", r.content);
        let r = exec
            .execute(&call(
                "write_scratch",
                serde_json::json!({"slug": "Foo", "body": "x"}),
            ))
            .await;
        assert!(r.content.starts_with("invalid_slug"), "got {}", r.content);
    }

    #[tokio::test]
    async fn dated_slug_is_blocked_on_write_but_delete_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;

        // write rejects trailing -YYYY-MM-DD slugs so dated-suffix accumulation
        // cannot recur via fresh writes.
        let r = exec
            .execute(&call(
                "write_scratch",
                serde_json::json!({"slug": "av-depot-2026-05-08", "body": "x"}),
            ))
            .await;
        assert!(r.content.starts_with("dated_slug"), "got {}", r.content);

        // delete still accepts dated slugs so the dreamer can prune the backlog.
        store
            .write_scratch(
                &FileKind::Scratch {
                    slug: "legacy-2026-05-08".into(),
                },
                "x",
                Some("12345"),
            )
            .await
            .unwrap();
        let r = exec
            .execute(&call(
                "delete_scratch",
                serde_json::json!({"slug": "legacy-2026-05-08"}),
            ))
            .await;
        assert_eq!(r.content, "ok");
    }

    #[tokio::test]
    async fn reserved_slug_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        let r = exec
            .execute(&call(
                "write_scratch",
                serde_json::json!({"slug": "system", "body": "x"}),
            ))
            .await;
        assert!(r.content.starts_with("reserved_slug"), "got {}", r.content);
    }

    #[tokio::test]
    async fn scratch_full_when_over_cap() {
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
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        let r = exec
            .execute(&call(
                "write_scratch",
                serde_json::json!({"slug": "first", "body": "x"}),
            ))
            .await;
        assert_eq!(r.content, "ok");
        let r = exec
            .execute(&call(
                "write_scratch",
                serde_json::json!({"slug": "second", "body": "x"}),
            ))
            .await;
        assert!(r.content.starts_with("scratch_full"), "got {}", r.content);
    }

    #[tokio::test]
    async fn write_quota_exhausts_after_n_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 1).await;
        let r1 = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/12345.md", "body": "a"}),
            ))
            .await;
        assert_eq!(r1.content, "ok");
        let r2 = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/12345.md", "body": "b"}),
            ))
            .await;
        assert!(
            r2.content.starts_with("write_quota_exhausted"),
            "got {}",
            r2.content
        );
    }

    #[tokio::test]
    async fn delete_scratch_blocked_for_non_creator_regular() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        // Pre-seed scratch file owned by user 99.
        store
            .write_scratch(
                &crate::ai::memory::types::FileKind::Scratch { slug: "qz".into() },
                "x",
                Some("99"),
            )
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        let r = exec
            .execute(&call("delete_scratch", serde_json::json!({"slug": "qz"})))
            .await;
        assert!(
            r.content.starts_with("permission_denied"),
            "got {}",
            r.content
        );
    }

    #[tokio::test]
    async fn dreamer_can_write_soul_lore_any_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = DreamerExecutor::new(DreamerExecutorOpts {
            store: store.clone(),
            max_writes_per_turn: 32,
        });
        for path in ["SOUL.md", "LORE.md", "users/77.md"] {
            let r = exec
                .execute(&call(
                    "write_file",
                    serde_json::json!({"path": path, "body": "x"}),
                ))
                .await;
            assert_eq!(r.content, "ok", "dreamer write {path}");
        }
    }
}
