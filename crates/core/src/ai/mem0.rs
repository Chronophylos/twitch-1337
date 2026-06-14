//! mem0 REST client. Four endpoints, behind a trait so tests fake it.
//!
//! OSS v2.0 endpoints (no `/v1/` prefix), auth via `X-API-Key`:
//! `POST /search`, `POST /memories`, `GET /memories?user_id=`,
//! `DELETE /memories?user_id=`, `DELETE /memories/{id}`.

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

// Suppress unused-import warnings for items that are part of the public API
// but not yet called from production code. Callers land in later tasks.
const _: fn() = || {
    let _: &dyn MemoryApi;
    let _: fn() -> ToolDefinition = forget_tool;
};

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
}
