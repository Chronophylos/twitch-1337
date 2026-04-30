//! Tool args + definitions for the v2 chat-turn and dreamer loops.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use llm::{ToolCall, ToolDefinition, ToolExecutor, ToolResultMessage};

use crate::ai::memory::sanitize::{
    PathError, SlugError, WritePath, check_body, parse_slug, parse_write_path, write_path_to_kind,
};
use crate::ai::memory::store_v2::{MemoryStore, WriteError};
use crate::ai::memory::types::{FileKind, Role};

const SAY_MAX_CHARS: usize = 500;

// ---------------------------------------------------------------------------
// Arg structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    pub path: String,
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteStateArgs {
    pub slug: String,
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteStateArgs {
    pub slug: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SayArgs {
    pub text: String,
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
        ToolDefinition::derived::<WriteStateArgs>(
            "write_state",
            "Create or overwrite a state file at state/<slug>.md. slug is lowercase a–z 0–9 dashes, ≤64 chars.",
        ),
        ToolDefinition::derived::<DeleteStateArgs>(
            "delete_state",
            "Remove a state file. Regulars may only delete state files they created.",
        ),
        ToolDefinition::derived::<SayArgs>(
            "say",
            "Append one chat line. Aim for ≤3 sentences per call; the app truncates >500 chars to ≤500 + …. Multiple calls produce multiple lines.",
        ),
    ]
}

pub fn dreamer_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::derived::<WriteFileArgs>(
            "write_file",
            "Overwrite SOUL.md / LORE.md / users/<id>.md.",
        ),
        ToolDefinition::derived::<WriteStateArgs>("write_state", "Overwrite state/<slug>.md."),
        ToolDefinition::derived::<DeleteStateArgs>("delete_state", "Remove a stale state file."),
    ]
}

// ---------------------------------------------------------------------------
// SayChannel — production uses mpsc; tests collect lines in a Vec
// ---------------------------------------------------------------------------

/// Sink for `say(text)` lines.
///
/// Production code wires an `mpsc::Sender<String>` from the !ai handler.
/// Tests use the collecting variant to inspect emitted lines without an IRC
/// connection.
#[derive(Clone)]
pub enum SayChannel {
    Mpsc(tokio::sync::mpsc::Sender<String>),
    Collect(Arc<Mutex<Vec<String>>>),
}

impl SayChannel {
    pub fn mpsc(tx: tokio::sync::mpsc::Sender<String>) -> Self {
        Self::Mpsc(tx)
    }

    pub fn collecting() -> Self {
        Self::Collect(Arc::new(Mutex::new(Vec::new())))
    }

    async fn send(&self, line: String) {
        match self {
            SayChannel::Mpsc(tx) => {
                let _ = tx.send(line).await;
            }
            SayChannel::Collect(buf) => buf.lock().await.push(line),
        }
    }
}

// ---------------------------------------------------------------------------
// ChatTurnExecutor
// ---------------------------------------------------------------------------

pub struct ChatTurnExecutorOpts {
    pub store: MemoryStore,
    pub speaker_user_id: String,
    pub speaker_display_name: String,
    pub speaker_role: Role,
    /// Maximum number of state/<slug>.md files that may exist at once.
    pub max_state_files: usize,
    /// Maximum write-class tool calls (write_file, write_state, delete_state)
    /// per turn. `say` is never counted.
    pub max_writes_per_turn: usize,
    pub say: SayChannel,
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

    /// Returns the collecting buffer when the executor was built with
    /// [`SayChannel::collecting`], otherwise `None`.
    pub fn say_collector(&self) -> Option<Arc<Mutex<Vec<String>>>> {
        match &self.opts.say {
            SayChannel::Collect(buf) => Some(buf.clone()),
            _ => None,
        }
    }

    fn role(&self) -> Role {
        self.opts.speaker_role
    }

    /// Returns `true` if the speaker is allowed to write to `path`.
    fn permitted_write_path(&self, path: &WritePath) -> bool {
        match (path, self.role()) {
            // Only Dreamer may write SOUL.md
            (WritePath::Soul, Role::Dreamer) => true,
            (WritePath::Soul, _) => false,
            // Moderator, Broadcaster, Dreamer may write LORE.md; Regular may not
            (WritePath::Lore, Role::Regular) => false,
            (WritePath::Lore, _) => true,
            // Regular may only write their own user file
            (WritePath::User { user_id }, Role::Regular) => user_id == &self.opts.speaker_user_id,
            // Higher roles can write any user file
            (WritePath::User { .. }, _) => true,
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
            Err(_) => return "invalid_arguments".into(),
        };
        let path = match parse_write_path(&args.path) {
            Ok(p) => p,
            Err(PathError) => return "invalid_path".into(),
        };
        if !self.permitted_write_path(&path) {
            return "permission_denied".into();
        }
        if check_body(&args.body).is_err() {
            return "invalid_body".into();
        }
        if !self.try_consume_write_quota() {
            return "write_quota_exhausted".into();
        }

        let kind = write_path_to_kind(path);
        // Pass the display name only for the speaker's own user file.
        let display = if matches!(&kind, FileKind::User { user_id } if user_id == &self.opts.speaker_user_id)
        {
            Some(self.opts.speaker_display_name.as_str())
        } else {
            None
        };
        match self.opts.store.write(&kind, &args.body, display).await {
            Ok(()) => "ok".into(),
            Err(WriteError::Full) => "file_full".into(),
            Err(WriteError::Io(e)) => format!("io_error: {e}"),
        }
    }

    async fn handle_write_state(&self, call: &ToolCall) -> String {
        let args: WriteStateArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments".into(),
        };
        let slug = match parse_slug(&args.slug) {
            Ok(s) => s,
            Err(SlugError::Invalid) => return "invalid_slug".into(),
            Err(SlugError::Reserved) => return "reserved_slug".into(),
        };
        if check_body(&args.body).is_err() {
            return "invalid_body".into();
        }

        // Enforce the state-count cap only for *new* files.
        // We detect "new" via filesystem existence (most reliable; avoids
        // parse-heuristic ambiguity when body is genuinely empty).
        let state_path = self
            .opts
            .store
            .memories_dir()
            .join(format!("state/{slug}.md"));
        let is_new = !tokio::fs::try_exists(&state_path).await.unwrap_or(false);
        if is_new {
            let listed = self.opts.store.list_state().await.unwrap_or_default();
            if listed.len() >= self.opts.max_state_files {
                return "state_full".into();
            }
        }

        if !self.try_consume_write_quota() {
            return "write_quota_exhausted".into();
        }

        match self
            .opts
            .store
            .write_state(
                &FileKind::State { slug },
                &args.body,
                Some(self.opts.speaker_user_id.as_str()),
            )
            .await
        {
            Ok(()) => "ok".into(),
            Err(WriteError::Full) => "file_full".into(),
            Err(WriteError::Io(e)) => format!("io_error: {e}"),
        }
    }

    async fn handle_delete_state(&self, call: &ToolCall) -> String {
        let args: DeleteStateArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments".into(),
        };
        let slug = match parse_slug(&args.slug) {
            Ok(s) => s,
            Err(SlugError::Invalid) => return "invalid_slug".into(),
            Err(SlugError::Reserved) => return "reserved_slug".into(),
        };

        // Check ownership before consuming quota.
        let owner = self
            .opts
            .store
            .read_kind(&FileKind::State { slug: slug.clone() })
            .await
            .ok()
            .and_then(|f| f.frontmatter.created_by);
        let allowed = match self.role() {
            Role::Regular => owner.as_deref() == Some(self.opts.speaker_user_id.as_str()),
            _ => true,
        };
        if !allowed {
            return "permission_denied".into();
        }

        if !self.try_consume_write_quota() {
            return "write_quota_exhausted".into();
        }

        match self.opts.store.delete_state(&slug).await {
            Ok(()) => "ok".into(),
            Err(e) => format!("io_error: {e}"),
        }
    }

    async fn handle_say(&self, call: &ToolCall) -> String {
        let args: SayArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments".into(),
        };
        let text = if args.text.chars().count() > SAY_MAX_CHARS {
            let truncated: String = args.text.chars().take(SAY_MAX_CHARS - 1).collect();
            format!("{truncated}\u{2026}") // U+2026 HORIZONTAL ELLIPSIS
        } else {
            args.text
        };
        // Collapse newlines so a single `say` call never splits an IRC PRIVMSG.
        let line = text.replace(['\n', '\r'], " ");
        self.opts.say.send(line).await;
        "ok".into()
    }
}

#[async_trait]
impl ToolExecutor for ChatTurnExecutor {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        let content = match call.name.as_str() {
            "write_file" => self.handle_write_file(call).await,
            "write_state" => self.handle_write_state(call).await,
            "delete_state" => self.handle_delete_state(call).await,
            "say" => self.handle_say(call).await,
            _ => "unknown_tool".to_string(),
        };
        ToolResultMessage::for_call(call, content)
    }
}

// ---------------------------------------------------------------------------
// DreamerExecutor
// ---------------------------------------------------------------------------

pub struct DreamerExecutorOpts {
    pub store: MemoryStore,
    pub max_state_files: usize,
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
                speaker_display_name: String::new(),
                speaker_role: Role::Dreamer,
                max_state_files: opts.max_state_files,
                max_writes_per_turn: opts.max_writes_per_turn,
                say: SayChannel::collecting(), // never used; dreamer never sees `say`.
            }),
        }
    }
}

#[async_trait]
impl ToolExecutor for DreamerExecutor {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        if call.name == "say" {
            return ToolResultMessage::for_call(call, "unknown_tool");
        }
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
    fn chat_turn_tools_has_four_named_tools() {
        let names: Vec<_> = chat_turn_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec!["write_file", "write_state", "delete_state", "say"]
        );
    }

    #[test]
    fn dreamer_tools_has_three_no_say() {
        let names: Vec<_> = dreamer_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["write_file", "write_state", "delete_state"]);
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
    use super::*;
    use crate::ai::memory::store_v2::MemoryStore;
    use crate::ai::memory::types::Caps;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: args,
            arguments_parse_error: None,
        }
    }

    async fn make_executor(
        role: Role,
        store: MemoryStore,
        max_state: usize,
        max_writes: usize,
    ) -> ChatTurnExecutor {
        ChatTurnExecutor::new(ChatTurnExecutorOpts {
            store,
            speaker_user_id: "12345".into(),
            speaker_display_name: "alice".into(),
            speaker_role: role,
            max_state_files: max_state,
            max_writes_per_turn: max_writes,
            say: SayChannel::collecting(),
        })
    }

    #[tokio::test]
    async fn regular_writes_own_user_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 16, 8).await;
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
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 16, 8).await;
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/99.md", "body": "hi"}),
            ))
            .await;
        assert_eq!(r.content, "permission_denied");
    }

    #[tokio::test]
    async fn regular_cannot_write_lore_or_soul() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 16, 8).await;
        for path in ["LORE.md", "SOUL.md"] {
            let r = exec
                .execute(&call(
                    "write_file",
                    serde_json::json!({"path": path, "body": "x"}),
                ))
                .await;
            assert_eq!(r.content, "permission_denied", "regular wrote {path}");
        }
    }

    #[tokio::test]
    async fn moderator_can_write_lore_but_not_soul() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Moderator, store.clone(), 16, 8).await;
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
        assert_eq!(r.content, "permission_denied");
    }

    #[tokio::test]
    async fn invalid_path_and_invalid_slug_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 16, 8).await;
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "../etc/passwd", "body": "x"}),
            ))
            .await;
        assert_eq!(r.content, "invalid_path");
        let r = exec
            .execute(&call(
                "write_state",
                serde_json::json!({"slug": "Foo", "body": "x"}),
            ))
            .await;
        assert_eq!(r.content, "invalid_slug");
    }

    #[tokio::test]
    async fn reserved_slug_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 16, 8).await;
        let r = exec
            .execute(&call(
                "write_state",
                serde_json::json!({"slug": "system", "body": "x"}),
            ))
            .await;
        assert_eq!(r.content, "reserved_slug");
    }

    #[tokio::test]
    async fn state_full_when_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 1, 8).await;
        let r = exec
            .execute(&call(
                "write_state",
                serde_json::json!({"slug": "first", "body": "x"}),
            ))
            .await;
        assert_eq!(r.content, "ok");
        let r = exec
            .execute(&call(
                "write_state",
                serde_json::json!({"slug": "second", "body": "x"}),
            ))
            .await;
        assert_eq!(r.content, "state_full");
    }

    #[tokio::test]
    async fn write_quota_exhausts_after_n_writes_but_say_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 16, 1).await;
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
        assert_eq!(r2.content, "write_quota_exhausted");
        let r3 = exec
            .execute(&call("say", serde_json::json!({"text": "still on"})))
            .await;
        assert_eq!(r3.content, "ok");
    }

    #[tokio::test]
    async fn delete_state_blocked_for_non_creator_regular() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        // Pre-seed state file owned by user 99.
        store
            .write_state(
                &crate::ai::memory::types::FileKind::State { slug: "qz".into() },
                "x",
                Some("99"),
            )
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 16, 8).await;
        let r = exec
            .execute(&call("delete_state", serde_json::json!({"slug": "qz"})))
            .await;
        assert_eq!(r.content, "permission_denied");
    }

    #[tokio::test]
    async fn say_truncates_over_500_chars() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 16, 8).await;
        let long = "x".repeat(600);
        let r = exec
            .execute(&call("say", serde_json::json!({"text": long})))
            .await;
        assert_eq!(r.content, "ok");
        let lines = exec.say_collector().unwrap().lock().await.clone();
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line.ends_with('\u{2026}'));
        assert!(line.chars().count() <= 500);
    }

    #[tokio::test]
    async fn dreamer_can_write_soul_lore_any_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = DreamerExecutor::new(DreamerExecutorOpts {
            store: store.clone(),
            max_state_files: 16,
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

    #[tokio::test]
    async fn dreamer_say_returns_unknown_tool() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), Caps::default())
            .await
            .unwrap();
        let exec = DreamerExecutor::new(DreamerExecutorOpts {
            store,
            max_state_files: 16,
            max_writes_per_turn: 32,
        });
        let r = exec
            .execute(&call("say", serde_json::json!({"text": "hi"})))
            .await;
        assert_eq!(r.content, "unknown_tool");
    }
}
