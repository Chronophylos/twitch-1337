//! mem0 REST client. Four endpoints, behind a trait so tests fake it.
//!
//! OSS v2.0 endpoints (no `/v1/` prefix), auth via `X-API-Key`:
//! `POST /search`, `POST /memories`, `GET /memories?user_id=`,
//! `DELETE /memories?user_id=`, `DELETE /memories/{id}`.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, eyre};
use llm::ToolDefinition;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};

pub const FORGET_TOOL_NAME: &str = "forget";

/// Which scope a `search` filters on. mem0 stores both ids on every memory and
/// filters by subset-match, so `Agent` is channel-wide and `User` is a subset.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    User(&'a str),
    Agent(&'a str),
}

/// One memory returned by search/list.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryHit {
    pub id: String,
    #[serde(default)]
    pub memory: String,
}

/// A chat message handed to `add` for extraction.
#[derive(Debug, Clone, Serialize)]
pub struct TurnMessage {
    pub role: String,
    pub content: String,
}

#[async_trait]
pub trait MemoryApi: Send + Sync {
    async fn search(&self, query: &str, scope: Scope<'_>) -> Result<Vec<MemoryHit>>;
    async fn add(
        &self,
        messages: &[TurnMessage],
        user_id: &str,
        agent_id: &str,
        metadata: serde_json::Value,
    ) -> Result<()>;
    async fn delete_user(&self, user_id: &str) -> Result<()>;
    async fn delete_one(&self, memory_id: &str) -> Result<()>;
    async fn list_user(&self, user_id: &str) -> Result<Vec<MemoryHit>>;
}

pub struct Mem0Client {
    http: reqwest::Client,
    base_url: String,
    api_key: SecretString,
    limit: usize,
}

impl Mem0Client {
    /// `base_url` is the mem0 server root (no trailing slash needed).
    pub fn new(
        base_url: &str,
        api_key: SecretString,
        timeout: Duration,
        limit: usize,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(crate::APP_USER_AGENT)
            .timeout(timeout)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            limit,
        })
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<MemoryHit>,
}

#[async_trait]
impl MemoryApi for Mem0Client {
    async fn search(&self, query: &str, scope: Scope<'_>) -> Result<Vec<MemoryHit>> {
        let mut body = serde_json::json!({ "query": query, "limit": self.limit });
        match scope {
            Scope::User(id) => body["user_id"] = id.into(),
            Scope::Agent(id) => body["agent_id"] = id.into(),
        }
        let resp = self
            .http
            .post(format!("{}/search", self.base_url))
            .header("X-API-Key", self.api_key.expose_secret())
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(eyre!("mem0 search {}", resp.status()));
        }
        Ok(resp.json::<SearchResponse>().await?.results)
    }

    async fn add(
        &self,
        messages: &[TurnMessage],
        user_id: &str,
        agent_id: &str,
        metadata: serde_json::Value,
    ) -> Result<()> {
        let body = serde_json::json!({
            "messages": messages,
            "user_id": user_id,
            "agent_id": agent_id,
            "metadata": metadata,
        });
        let resp = self
            .http
            .post(format!("{}/memories", self.base_url))
            .header("X-API-Key", self.api_key.expose_secret())
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(eyre!("mem0 add {}", resp.status()));
        }
        Ok(())
    }

    async fn delete_user(&self, user_id: &str) -> Result<()> {
        let resp = self
            .http
            .delete(format!("{}/memories", self.base_url))
            .query(&[("user_id", user_id)])
            .header("X-API-Key", self.api_key.expose_secret())
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(eyre!("mem0 delete_user {}", resp.status()));
        }
        Ok(())
    }

    async fn delete_one(&self, memory_id: &str) -> Result<()> {
        let resp = self
            .http
            .delete(format!("{}/memories/{memory_id}", self.base_url))
            .header("X-API-Key", self.api_key.expose_secret())
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(eyre!("mem0 delete_one {}", resp.status()));
        }
        Ok(())
    }

    async fn list_user(&self, user_id: &str) -> Result<Vec<MemoryHit>> {
        let resp = self
            .http
            .get(format!("{}/memories", self.base_url))
            .query(&[("user_id", user_id)])
            .header("X-API-Key", self.api_key.expose_secret())
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(eyre!("mem0 list {}", resp.status()));
        }
        // OSS returns a bare array for GET /memories.
        Ok(resp.json::<Vec<MemoryHit>>().await?)
    }
}

/// The single memory tool: erases the speaker's own memories. No args.
pub fn forget_tool() -> ToolDefinition {
    ToolDefinition {
        name: FORGET_TOOL_NAME.into(),
        description: "Erase everything you remember about the user you are currently talking to. \
            Use only when they explicitly ask you to forget them. Takes no arguments."
            .into(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
    }
}

/// One-shot migration of legacy markdown memories into mem0. Reads
/// `memories/users/<id>.md` (→ user-scoped) and `memories/LORE.md`
/// (→ channel-scoped) off disk, strips frontmatter, and `add`s each body so
/// mem0 distils it. SOUL.md is intentionally skipped (folded into the system
/// prompt by hand). Returns `Err` on the first failed `add` so the caller can
/// skip the sentinel and retry next boot.
pub async fn migrate_legacy_memories(
    m: &dyn MemoryApi,
    data_dir: &Path,
    channel: &str,
) -> eyre::Result<()> {
    fn strip_frontmatter(raw: &str) -> &str {
        // Files are `---\n<fm>\n---\n<body>`; split on the closing delimiter.
        raw.split_once("\n---\n").map(|(_, b)| b).unwrap_or(raw)
    }
    let one = |body: &str| {
        vec![TurnMessage {
            role: "user".into(),
            content: body.to_owned(),
        }]
    };

    // Per-user character sheets → user-scoped memories.
    let users_dir = data_dir.join("memories/users");
    if let Ok(mut rd) = tokio::fs::read_dir(&users_dir).await {
        while let Some(ent) = rd.next_entry().await? {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let raw = tokio::fs::read_to_string(&path).await?;
            let body = strip_frontmatter(&raw).trim();
            if body.is_empty() {
                continue;
            }
            m.add(&one(body), id, channel, serde_json::json!({}))
                .await?;
        }
    }

    // Channel LORE → channel-scoped. mem0 requires a user_id on add; use a
    // synthetic id so the fact stays channel-wide without a personal owner.
    // ponytail: synthetic "lore-<channel>" id; revisit if mem0 gains agent-only writes.
    let lore = data_dir.join("memories/LORE.md");
    if let Ok(raw) = tokio::fs::read_to_string(&lore).await {
        let body = strip_frontmatter(&raw).trim();
        if !body.is_empty() {
            m.add(
                &one(body),
                &format!("lore-{channel}"),
                channel,
                serde_json::json!({}),
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> Mem0Client {
        crate::install_crypto_provider();
        Mem0Client::new(
            &server.uri(),
            SecretString::from("k"),
            Duration::from_secs(5),
            8,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn search_user_scope_posts_user_id_and_parses_results() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("x-api-key", "k"))
            .and(body_partial_json(
                serde_json::json!({ "user_id": "42", "limit": 8 }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{ "id": "m1", "memory": "likes cats" }]
            })))
            .mount(&server)
            .await;
        let hits = client(&server)
            .search("q", Scope::User("42"))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m1");
        assert_eq!(hits[0].memory, "likes cats");
    }

    #[tokio::test]
    async fn search_agent_scope_posts_agent_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(body_partial_json(
                serde_json::json!({ "agent_id": "channel" }),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [] })),
            )
            .mount(&server)
            .await;
        let hits = client(&server)
            .search("q", Scope::Agent("channel"))
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn delete_user_hits_query_param() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/memories"))
            .and(query_param("user_id", "42"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        client(&server).delete_user("42").await.unwrap();
    }

    #[tokio::test]
    async fn non_success_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(
            client(&server)
                .search("q", Scope::Agent("c"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn add_posts_messages_and_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/memories"))
            .and(header("x-api-key", "k"))
            .and(body_partial_json(serde_json::json!({
                "user_id": "42",
                "agent_id": "mychannel",
                "messages": [{ "role": "user", "content": "hello" }]
            })))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        client(&server)
            .add(
                &[TurnMessage {
                    role: "user".into(),
                    content: "hello".into(),
                }],
                "42",
                "mychannel",
                serde_json::json!({}),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_user_parses_bare_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/memories"))
            .and(query_param("user_id", "42"))
            .and(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "m1", "memory": "likes dogs" }
            ])))
            .mount(&server)
            .await;
        let hits = client(&server).list_user("42").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m1");
        assert_eq!(hits[0].memory, "likes dogs");
    }

    // --- migrate_legacy_memories tests ---

    struct FakeMemory {
        calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl FakeMemory {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn recorded(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl MemoryApi for FakeMemory {
        async fn search(&self, _query: &str, _scope: Scope<'_>) -> eyre::Result<Vec<MemoryHit>> {
            Ok(vec![])
        }

        async fn add(
            &self,
            _messages: &[TurnMessage],
            user_id: &str,
            agent_id: &str,
            _metadata: serde_json::Value,
        ) -> eyre::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((user_id.to_owned(), agent_id.to_owned()));
            Ok(())
        }

        async fn delete_user(&self, _user_id: &str) -> eyre::Result<()> {
            Ok(())
        }

        async fn delete_one(&self, _memory_id: &str) -> eyre::Result<()> {
            Ok(())
        }

        async fn list_user(&self, _user_id: &str) -> eyre::Result<Vec<MemoryHit>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn migrate_legacy_memories_adds_user_and_lore_skips_soul() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        // Create memories/users/7.md with frontmatter + body.
        let users_dir = root.join("memories/users");
        std::fs::create_dir_all(&users_dir).unwrap();
        std::fs::write(
            users_dir.join("7.md"),
            "---\ncreated: 2025-01-01\n---\nUser 7 loves cats.",
        )
        .unwrap();

        // Create memories/LORE.md with frontmatter + body.
        let mem_dir = root.join("memories");
        std::fs::write(
            mem_dir.join("LORE.md"),
            "---\nversion: 1\n---\nChannel lore goes here.",
        )
        .unwrap();

        // Create memories/SOUL.md — must NOT be migrated.
        std::fs::write(mem_dir.join("SOUL.md"), "---\n---\nBot persona.").unwrap();

        let fake = FakeMemory::new();
        super::migrate_legacy_memories(&fake, root, "chan")
            .await
            .unwrap();

        let calls = fake.recorded();
        assert_eq!(
            calls.len(),
            2,
            "expected exactly two add() calls, got {calls:?}"
        );

        let user_call = calls.iter().find(|(uid, _)| uid == "7");
        assert!(user_call.is_some(), "missing add for user_id='7'");
        assert_eq!(
            user_call.unwrap().1,
            "chan",
            "user call agent_id must be channel"
        );

        let lore_call = calls.iter().find(|(uid, _)| uid == "lore-chan");
        assert!(lore_call.is_some(), "missing add for user_id='lore-chan'");
        assert_eq!(
            lore_call.unwrap().1,
            "chan",
            "lore call agent_id must be channel"
        );

        // Confirm SOUL.md not added.
        assert!(
            calls.iter().all(|(uid, _)| uid != "SOUL"),
            "SOUL.md must not be migrated"
        );
    }

    #[tokio::test]
    async fn migrate_legacy_memories_missing_dirs_are_silent() {
        // No memories directories at all — should succeed with zero adds.
        let dir = tempfile::TempDir::new().unwrap();
        let fake = FakeMemory::new();
        super::migrate_legacy_memories(&fake, dir.path(), "chan")
            .await
            .unwrap();
        assert!(fake.recorded().is_empty());
    }

    #[tokio::test]
    async fn migrate_legacy_memories_skips_empty_bodies() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let users_dir = root.join("memories/users");
        std::fs::create_dir_all(&users_dir).unwrap();
        // File with frontmatter but empty body (only whitespace after `---`).
        std::fs::write(users_dir.join("9.md"), "---\ntitle: empty\n---\n   ").unwrap();

        let fake = FakeMemory::new();
        super::migrate_legacy_memories(&fake, root, "chan")
            .await
            .unwrap();
        assert!(fake.recorded().is_empty(), "empty bodies must not be added");
    }
}
