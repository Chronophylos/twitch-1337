//! Tool args + definitions for the v2 chat-turn and dreamer loops.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use llm::{ToolCall, ToolDefinition, ToolExecutor, ToolResultMessage};

use crate::ai::memory::inject::{FenceLabel, fence_block};
use crate::ai::memory::sanitize::{
    PathError, SlugError, WritePath, check_body, parse_read_path, parse_write_path,
    validate_scratch_slug, write_path_to_kind,
};
use crate::ai::memory::store::{MemoryStore, UserIdentity, WriteError};
use crate::ai::memory::types::{FileKind, Role};
use crate::pronouns::PronounsClient;

/// Canonical LLM string for Stable Slug charset / length / date failures.
const INVALID_SLUG_MSG: &str = "invalid_slug: slug must be lowercase a-z, 0-9, and dashes, up to 64 chars, and must not end in a -YYYY-MM-DD date suffix";
const RESERVED_SLUG_MSG: &str = "reserved_slug: that slug name is reserved";
/// Operational failures stay in the log (ADR-0012 rule 3): the chain names
/// data-dir paths, and the model speaks in public chat.
const IO_ERROR_MSG: &str = "io_error: the memory store could not be accessed, try again later";

fn map_slug_err(err: SlugError) -> String {
    match err {
        SlugError::Invalid => INVALID_SLUG_MSG.into(),
        SlugError::Reserved => RESERVED_SLUG_MSG.into(),
    }
}

fn map_write_err(err: WriteError) -> String {
    match err {
        // Name both numbers: without them the model can only blind-guess how
        // much to trim, and it tends to over-trim (drop facts) to be safe.
        WriteError::Full { limit, actual } => format!(
            "file_full: the file is {actual} bytes with frontmatter, cap is {limit}. \
             Rewrite it under the cap without dropping facts you can compress instead."
        ),
        WriteError::ScratchFull => {
            "scratch_full: the maximum number of scratch files already exists".into()
        }
        WriteError::InvalidSlug => INVALID_SLUG_MSG.into(),
        WriteError::ReservedSlug => RESERVED_SLUG_MSG.into(),
        WriteError::Io(error) => {
            tracing::error!(?error, "memory tool write failed");
            IO_ERROR_MSG.into()
        }
    }
}

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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMemoryArgs {
    pub path: String,
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

pub fn chat_turn_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::derived::<WriteFileArgs>(
            "write_file",
            "Überschreib eine Memory-Datei (SOUL.md, LORE.md oder users/<id>.md). body ist der komplette neue Fließtext, kein Diff; das Frontmatter macht der Store. Rechte hängen an der Rolle der sprechenden Person.",
        ),
        ToolDefinition::derived::<WriteScratchArgs>(
            "write_scratch",
            "Leg eine Scratch-Datei unter scratch/<slug>.md an oder überschreib sie. slug ist klein a–z, 0–9 und Bindestriche, maximal 64 Zeichen.",
        ),
        ToolDefinition::derived::<DeleteScratchArgs>(
            "delete_scratch",
            "Lösch eine Scratch-Datei. Regulars dürfen nur Scratch-Dateien löschen, die sie selbst angelegt haben.",
        ),
    ]
}

/// Grok's tools: `read_memory` only. Grok gets a paths-only manifest instead of
/// injected bodies, so it needs a way to pull one; Aurora already has every body
/// in her prompt and would only burn a round-trip. Memory is read-only for grok
/// — writes belong to Aurora and the dreamer.
pub fn grok_chat_turn_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition::derived::<ReadMemoryArgs>(
        "read_memory",
        "Lies eine Memory-Datei aus dem Memory-Index dieses Turns. path ist ein exakter Pfad aus diesem Index (LORE.md, users/<id>.md oder scratch/<slug>.md). Gibt den Body als Fence zurück. Pfade außerhalb des Index werden abgelehnt.",
    )]
}

pub fn dreamer_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::derived::<WriteFileArgs>(
            "write_file",
            "Überschreib SOUL.md / LORE.md / users/<id>.md.",
        ),
        ToolDefinition::derived::<WriteScratchArgs>(
            "write_scratch",
            "Überschreib scratch/<slug>.md.",
        ),
        ToolDefinition::derived::<DeleteScratchArgs>(
            "delete_scratch",
            "Lösch eine veraltete Scratch-Datei.",
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
    /// Paths `read_memory` may serve, from the turn's memory manifest. Empty for
    /// Aurora turns, which have no `read_memory` tool registered at all.
    pub readable_paths: BTreeSet<String>,
    /// Nonce for fencing `read_memory` results. Same nonce as the turn's
    /// injected blocks so the model sees one consistent fence scheme.
    pub inject_nonce: String,
    /// `user_id -> (login, display_name)` for everyone in this turn's chat
    /// history. Lets a write to *someone else's* sheet still stamp identity —
    /// otherwise a sheet first created for a third party is born nameless and
    /// stays that way, since every later write only ever preserves.
    pub known_identities: HashMap<String, (String, String)>,
    /// Pronouns lookup, consulted only when a write resolves the subject's
    /// login. `None` (dreamer, tests) skips the lookup and preserves whatever
    /// is on disk.
    pub pronouns: Option<Arc<PronounsClient>>,
    /// User ids that already have a sheet on disk. Together with the speaker's
    /// own id and `known_identities`, this is the full set `write_file` may
    /// target. Anything else is an id the model made up: nothing in the turn
    /// carries a Twitch id for it, so it cannot be real.
    pub writable_user_ids: BTreeSet<String>,
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

    /// `Some(id)` when `path` targets a user sheet whose id appears nowhere
    /// this turn: not the speaker, not in the chat history, and no file on
    /// disk. The dreamer reads logins out of the transcript and used to mint
    /// plausible-looking numbers for them, which produced duplicate sheets
    /// under ids no Twitch account owns.
    fn unknown_user_id<'a>(&self, path: &'a WritePath) -> Option<&'a str> {
        let WritePath::User { user_id } = path else {
            return None;
        };
        let known = user_id == &self.opts.speaker_user_id
            || self.opts.known_identities.contains_key(user_id)
            || self.opts.writable_user_ids.contains(user_id);
        (!known).then_some(user_id.as_str())
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
        if let Some(user_id) = self.unknown_user_id(&path) {
            return format!(
                "unknown_user_id: {user_id} has no memory file and is not a Twitch id from this turn. \
                 Never invent a user id; write only to ids you were given."
            );
        }
        if check_body(&args.body).is_err() {
            return "invalid_body: body contains a reserved marker (frontmatter fence, path header, or carriage return)".into();
        }
        if !self.try_consume_write_quota() {
            return "write_quota_exhausted: the write limit for this turn is used up".into();
        }

        let kind = write_path_to_kind(path);
        // Identity for the sheet's *subject*: the speaker for their own file,
        // otherwise whatever this turn's chat history knows about that id.
        // Still `None` for a stranger, which lets the store preserve disk.
        let (login, display) = match &kind {
            FileKind::User { user_id } if user_id == &self.opts.speaker_user_id => (
                Some(self.opts.speaker_login.as_str()),
                Some(self.opts.speaker_display_name.as_str()),
            ),
            FileKind::User { user_id } => match self.opts.known_identities.get(user_id) {
                Some((l, d)) => (Some(l.as_str()), Some(d.as_str())),
                None => (None, None),
            },
            _ => (None, None),
        };
        // Only ask the pronouns service when we actually resolved a login, so
        // the call rides along with the (rare) writes rather than every turn.
        // Unreachable service or unset pronouns → `None` → prior value kept.
        let pronouns = match (&self.opts.pronouns, login) {
            (Some(client), Some(login)) => client.lookup(login).await,
            _ => None,
        };
        let ident = UserIdentity {
            username: login,
            display_name: display,
            pronouns: pronouns.as_deref(),
        };
        match self.opts.store.write(&kind, &args.body, ident).await {
            Ok(()) => "ok".into(),
            Err(e) => map_write_err(e),
        }
    }

    async fn handle_write_scratch(&self, call: &ToolCall) -> String {
        let args: WriteScratchArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments: could not parse the tool arguments".into(),
        };
        // Same predicate the store enforces — checked here only so invalid
        // slugs do not burn write quota before the store rejects them.
        if let Err(e) = validate_scratch_slug(&args.slug) {
            return map_slug_err(e);
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
                &FileKind::Scratch { slug: args.slug },
                &args.body,
                Some(self.opts.speaker_user_id.as_str()),
            )
            .await
        {
            Ok(()) => "ok".into(),
            Err(e) => map_write_err(e),
        }
    }

    async fn handle_delete_scratch(&self, call: &ToolCall) -> String {
        let args: DeleteScratchArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments: could not parse the tool arguments".into(),
        };
        if let Err(e) = validate_scratch_slug(&args.slug) {
            return map_slug_err(e);
        }

        // Check ownership before consuming quota.
        let owner = match self.opts.store.read_scratch(&args.slug).await {
            Ok(f) => f.frontmatter.created_by,
            Err(e) => return map_write_err(e),
        };
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

        match self.opts.store.delete_scratch(&args.slug).await {
            Ok(()) => "ok".into(),
            Err(e) => map_write_err(e),
        }
    }

    async fn handle_read_memory(&self, call: &ToolCall) -> String {
        let args: ReadMemoryArgs = match call.parse_args() {
            Ok(a) => a,
            Err(_) => return "invalid_arguments: could not parse the tool arguments".into(),
        };
        // Authorization is the manifest, not the parser: a well-formed path the
        // manifest never listed is refused before the store is touched.
        if !self.opts.readable_paths.contains(&args.path) {
            let listed = self
                .opts
                .readable_paths
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            return format!(
                "invalid_path: {} is not in this turn's memory index. Readable paths: {listed}",
                args.path
            );
        }
        let kind = match parse_read_path(&args.path) {
            Ok(k) => k,
            Err(PathError) => {
                return "invalid_path: path must be LORE.md, users/<numeric-id>.md, or scratch/<slug>.md".into();
            }
        };
        let file = match self.opts.store.read_kind(&kind).await {
            Ok(f) => f,
            Err(error) => {
                tracing::error!(?error, path = %args.path, "read_memory failed");
                return IO_ERROR_MSG.into();
            }
        };
        let label = match &kind {
            FileKind::Soul => FenceLabel::Soul,
            FileKind::Lore => FenceLabel::Lore,
            FileKind::User { user_id } => FenceLabel::User {
                id: user_id,
                login: file.frontmatter.username.as_deref(),
                display_name: file.frontmatter.display_name.as_deref(),
                pronouns: file.frontmatter.pronouns.as_deref(),
            },
            FileKind::Scratch { slug } => FenceLabel::Scratch { slug },
        };
        let body = crate::ai::memory::inject::body_with_links(&file.frontmatter, &file.body);
        fence_block(label, &self.opts.inject_nonce, &body)
    }
}

#[async_trait]
impl ToolExecutor for ChatTurnExecutor {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        let content = match call.name.as_str() {
            "write_file" => self.handle_write_file(call).await,
            "write_scratch" => self.handle_write_scratch(call).await,
            "delete_scratch" => self.handle_delete_scratch(call).await,
            "read_memory" => self.handle_read_memory(call).await,
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
    /// Every user sheet that exists on disk. The dreamer has no speaker and no
    /// chat history, so this is the whole set of sheets it may rewrite.
    pub known_user_ids: BTreeSet<String>,
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
                readable_paths: BTreeSet::new(),
                inject_nonce: String::new(),
                known_identities: HashMap::new(),
                pronouns: None,
                writable_user_ids: opts.known_user_ids,
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
    fn chat_turn_and_dreamer_advertise_the_three_write_tools() {
        for tools in [chat_turn_tools(), dreamer_tools()] {
            let names: Vec<_> = tools.into_iter().map(|t| t.name).collect();
            assert_eq!(names, vec!["write_file", "write_scratch", "delete_scratch"]);
        }
    }

    #[test]
    fn grok_tools_are_read_only() {
        let names: Vec<_> = grok_chat_turn_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["read_memory"]);
    }
}

#[cfg(test)]
mod exec_tests {
    use std::sync::Arc;

    use super::*;
    use crate::ai::memory::store::MemoryStore;
    use crate::ai::memory::types::Caps;
    use crate::settings::Settings;

    fn test_handle() -> crate::settings::SettingsHandle {
        crate::settings::SettingsHandle::from_settings(Settings::compiled_defaults())
    }

    fn handle_with_caps(caps: Caps) -> crate::settings::SettingsHandle {
        let mut s = Settings::compiled_defaults();
        s.ai.memory.soul_bytes = caps.soul_bytes;
        s.ai.memory.lore_bytes = caps.lore_bytes;
        s.ai.memory.user_bytes = caps.user_bytes;
        s.ai.memory.scratch_bytes = caps.scratch_bytes;
        s.ai.memory.max_scratch_files = caps.max_scratch_files;
        crate::settings::SettingsHandle::from_settings(s)
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
            readable_paths: Default::default(),
            inject_nonce: "n0".into(),
            known_identities: HashMap::new(),
            pronouns: None,
            writable_user_ids: BTreeSet::from(["12345".to_string()]),
        })
    }

    #[tokio::test]
    async fn mod_write_to_third_party_sheet_stamps_identity_from_chat_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = ChatTurnExecutor::new(ChatTurnExecutorOpts {
            store: store.clone(),
            speaker_user_id: "12345".into(),
            speaker_login: "alice".into(),
            speaker_display_name: "Alice".into(),
            speaker_role: Role::Moderator,
            max_writes_per_turn: 8,
            readable_paths: Default::default(),
            inject_nonce: "n0".into(),
            pronouns: None,
            known_identities: HashMap::from([(
                "99".to_string(),
                ("bobby".to_string(), "BobbyTables".to_string()),
            )]),
            writable_user_ids: BTreeSet::new(),
        });
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/99.md", "body": "hi"}),
            ))
            .await;
        assert_eq!(r.content, "ok");
        let mf = store
            .read_kind(&FileKind::User {
                user_id: "99".into(),
            })
            .await
            .unwrap();
        assert_eq!(mf.frontmatter.username.as_deref(), Some("bobby"));
        assert_eq!(mf.frontmatter.display_name.as_deref(), Some("BobbyTables"));
    }

    #[tokio::test]
    async fn write_to_an_id_from_nowhere_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        // Moderator: role is not what stops this write, the unknown id is.
        let exec = ChatTurnExecutor::new(ChatTurnExecutorOpts {
            store: store.clone(),
            speaker_user_id: "12345".into(),
            speaker_login: "alice".into(),
            speaker_display_name: "Alice".into(),
            speaker_role: Role::Moderator,
            max_writes_per_turn: 8,
            readable_paths: Default::default(),
            inject_nonce: "n0".into(),
            known_identities: HashMap::new(),
            pronouns: None,
            writable_user_ids: BTreeSet::new(),
        });
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/10000000.md", "body": "hi"}),
            ))
            .await;
        assert!(
            r.content.starts_with("unknown_user_id:"),
            "got {}",
            r.content
        );
    }

    #[tokio::test]
    async fn own_sheet_write_stamps_pronouns_and_a_dead_service_preserves_them() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let kind = FileKind::User {
            user_id: "12345".into(),
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pronouns"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sheher": {"name": "sheher", "subject": "She", "object": "Her", "singular": false},
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/users/alice"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"id": "12345", "login": "alice", "pronoun_id": "sheher"}),
            ))
            .mount(&server)
            .await;

        let make = |base: String| {
            ChatTurnExecutor::new(ChatTurnExecutorOpts {
                store: store.clone(),
                speaker_user_id: "12345".into(),
                speaker_login: "alice".into(),
                speaker_display_name: "Alice".into(),
                speaker_role: Role::Regular,
                max_writes_per_turn: 8,
                readable_paths: Default::default(),
                inject_nonce: "n0".into(),
                known_identities: HashMap::new(),
                pronouns: Some(Arc::new(PronounsClient::with_base_url(
                    reqwest::Client::new(),
                    base,
                ))),
                writable_user_ids: BTreeSet::new(),
            })
        };
        let write = serde_json::json!({"path": "users/12345.md", "body": "hi"});

        let r = make(server.uri())
            .execute(&call("write_file", write.clone()))
            .await;
        assert_eq!(r.content, "ok");
        assert_eq!(
            store
                .read_kind(&kind)
                .await
                .unwrap()
                .frontmatter
                .pronouns
                .as_deref(),
            Some("She/Her")
        );

        // Service unreachable on the next write: the stamped value stays put
        // rather than being blanked by a failed lookup.
        drop(server);
        let r = make("http://127.0.0.1:1/v1".to_string())
            .execute(&call("write_file", write))
            .await;
        assert_eq!(r.content, "ok");
        assert_eq!(
            store
                .read_kind(&kind)
                .await
                .unwrap()
                .frontmatter
                .pronouns
                .as_deref(),
            Some("She/Her")
        );
    }

    #[tokio::test]
    async fn regular_cannot_write_lore_soul_or_other_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = make_executor(Role::Regular, store.clone(), 8).await;
        for path in ["LORE.md", "SOUL.md", "users/99.md"] {
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
    async fn invalid_path_and_slug_map_to_error_codes() {
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
    async fn read_memory_returns_a_fenced_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        store
            .write(
                &FileKind::Lore,
                "the channel hates mondays",
                UserIdentity::default(),
            )
            .await
            .unwrap();

        let exec = ChatTurnExecutor::new(ChatTurnExecutorOpts {
            store: store.clone(),
            speaker_user_id: "111".into(),
            speaker_login: "alice".into(),
            speaker_display_name: "Alice".into(),
            speaker_role: Role::Regular,
            max_writes_per_turn: 4,
            readable_paths: ["LORE.md".to_string()].into_iter().collect(),
            inject_nonce: "cafebabecafebabe".into(),
            known_identities: HashMap::new(),
            pronouns: None,
            writable_user_ids: BTreeSet::new(),
        });

        let out = exec
            .execute(&call("read_memory", serde_json::json!({"path": "LORE.md"})))
            .await;

        assert!(out.content.contains("the channel hates mondays"));
        assert!(
            out.content
                .contains("<<<FILE kind=lore nonce=cafebabecafebabe>>>")
        );
        assert!(out.content.contains("<<<ENDFILE nonce=cafebabecafebabe>>>"));
    }

    #[tokio::test]
    async fn read_memory_refuses_a_path_outside_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        store
            .write(
                &FileKind::Soul,
                "aurora self description",
                UserIdentity::default(),
            )
            .await
            .unwrap();

        let exec = ChatTurnExecutor::new(ChatTurnExecutorOpts {
            store: store.clone(),
            speaker_user_id: "111".into(),
            speaker_login: "alice".into(),
            speaker_display_name: "Alice".into(),
            speaker_role: Role::Regular,
            max_writes_per_turn: 4,
            readable_paths: ["LORE.md".to_string()].into_iter().collect(),
            inject_nonce: "n0".into(),
            known_identities: HashMap::new(),
            pronouns: None,
            writable_user_ids: BTreeSet::new(),
        });

        let out = exec
            .execute(&call("read_memory", serde_json::json!({"path": "SOUL.md"})))
            .await;

        assert!(out.content.starts_with("invalid_path:"));
        // The refusal must not leak the body it refused to serve.
        assert!(!out.content.contains("aurora self description"));
    }

    #[tokio::test]
    async fn read_memory_does_not_consume_write_quota() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        store
            .write(&FileKind::Lore, "body", UserIdentity::default())
            .await
            .unwrap();

        let exec = ChatTurnExecutor::new(ChatTurnExecutorOpts {
            store: store.clone(),
            speaker_user_id: "111".into(),
            speaker_login: "alice".into(),
            speaker_display_name: "Alice".into(),
            speaker_role: Role::Regular,
            max_writes_per_turn: 1,
            readable_paths: ["LORE.md".to_string()].into_iter().collect(),
            inject_nonce: "n0".into(),
            known_identities: HashMap::new(),
            pronouns: None,
            writable_user_ids: BTreeSet::new(),
        });

        for _ in 0..3 {
            let out = exec
                .execute(&call("read_memory", serde_json::json!({"path": "LORE.md"})))
                .await;
            assert!(!out.content.contains("write_quota_exhausted"));
        }

        // The one write is still available after three reads.
        let out = exec
            .execute(&call(
                "write_scratch",
                serde_json::json!({"slug": "quiz", "body": "round 1"}),
            ))
            .await;
        assert_eq!(out.content, "ok");
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
    async fn dreamer_refuses_a_user_id_with_no_sheet_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        store
            .write(
                &FileKind::User {
                    user_id: "77".into(),
                },
                "existing sheet",
                UserIdentity::of("bob", "Bob"),
            )
            .await
            .unwrap();
        let exec = DreamerExecutor::new(DreamerExecutorOpts {
            store: store.clone(),
            max_writes_per_turn: 32,
            known_user_ids: store.list_user_ids().await.unwrap(),
        });

        // Invented id: nothing on disk, no chat context to vouch for it.
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/10000000.md", "body": "x"}),
            ))
            .await;
        assert!(
            r.content.starts_with("unknown_user_id:"),
            "got {}",
            r.content
        );
        assert!(
            !dir.path().join("memories/users/10000000.md").exists(),
            "refused write must not create the file"
        );

        // The real sheet still writes.
        let r = exec
            .execute(&call(
                "write_file",
                serde_json::json!({"path": "users/77.md", "body": "x"}),
            ))
            .await;
        assert_eq!(r.content, "ok");
    }

    #[tokio::test]
    async fn dreamer_can_write_soul_lore_any_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path(), test_handle()).await.unwrap();
        let exec = DreamerExecutor::new(DreamerExecutorOpts {
            store: store.clone(),
            max_writes_per_turn: 32,
            known_user_ids: BTreeSet::from(["77".to_string()]),
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
