//! pronouns.alejo.io lookup — the de-facto Twitch pronouns service that
//! chat clients read. Users declare their own pronouns there, so this is the
//! only source we trust for the `pronouns` field on a user memory sheet:
//! nobody else gets to guess on their behalf.
//!
//! Every failure path returns `None`. A memory write must never fail, or even
//! wait long, because a third-party service is having a bad day.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::OnceCell;

use crate::APP_USER_AGENT;

const DEFAULT_BASE_URL: &str = "https://api.pronouns.alejo.io/v1";
const TIMEOUT: Duration = Duration::from_secs(3);

/// One entry of the service's pronoun table, e.g.
/// `heyhim -> {subject: "He", object: "Him", singular: false}`.
#[derive(Debug, Clone, Deserialize)]
struct PronounDef {
    subject: String,
    object: String,
    /// `true` for entries that render as one word ("Any", "Other", "Ask").
    singular: bool,
}

#[derive(Debug, Deserialize)]
struct UserPronouns {
    pronoun_id: String,
}

pub struct PronounsClient {
    http: reqwest::Client,
    base_url: String,
    /// The id -> definition table. Fetched once per process: it's a static
    /// list of ~15 entries that changes about never, so a per-lookup fetch
    /// would double every request for nothing.
    table: OnceCell<HashMap<String, PronounDef>>,
}

impl PronounsClient {
    pub fn new() -> reqwest::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(TIMEOUT)
            .build()?;
        Ok(Self::with_base_url(http, DEFAULT_BASE_URL))
    }

    /// Test hook: custom base URL (e.g. wiremock).
    pub fn with_base_url(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            table: OnceCell::new(),
        }
    }

    async fn table(&self) -> Option<&HashMap<String, PronounDef>> {
        self.table
            .get_or_try_init(|| async {
                self.http
                    .get(format!("{}/pronouns", self.base_url))
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<HashMap<String, PronounDef>>()
                    .await
            })
            .await
            .inspect_err(|error| tracing::debug!(?error, "pronouns table fetch failed"))
            .ok()
    }

    /// Display form for `login`'s declared pronouns ("He/Him", "They/Them",
    /// "Any"), or `None` when unset, unknown, or the service is unreachable.
    pub async fn lookup(&self, login: &str) -> Option<String> {
        let login = login.trim().to_ascii_lowercase();
        if login.is_empty() {
            return None;
        }
        let resp = self
            .http
            .get(format!("{}/users/{login}", self.base_url))
            .send()
            .await
            .inspect_err(|error| tracing::debug!(?error, %login, "pronouns lookup failed"))
            .ok()?;
        // 404 is the normal "this user has not set pronouns" answer.
        let user: UserPronouns = resp.error_for_status().ok()?.json().await.ok()?;
        let def = self.table().await?.get(&user.pronoun_id)?;
        Some(if def.singular {
            def.subject.clone()
        } else {
            format!("{}/{}", def.subject, def.object)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn table_json() -> serde_json::Value {
        serde_json::json!({
            "heyhim": {"name": "heyhim", "subject": "He", "object": "Him", "singular": false},
            "any": {"name": "any", "subject": "Any", "object": "Any", "singular": true},
        })
    }

    async fn client(server: &MockServer) -> PronounsClient {
        crate::install_crypto_provider();
        Mock::given(method("GET"))
            .and(path("/pronouns"))
            .respond_with(ResponseTemplate::new(200).set_body_json(table_json()))
            .mount(server)
            .await;
        PronounsClient::with_base_url(reqwest::Client::new(), server.uri())
    }

    #[tokio::test]
    async fn renders_paired_and_singular_forms() {
        let server = MockServer::start().await;
        let c = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/users/alice"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"id": "1", "login": "alice", "pronoun_id": "heyhim"}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/users/bob"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"id": "2", "login": "bob", "pronoun_id": "any"}),
                ),
            )
            .mount(&server)
            .await;
        assert_eq!(c.lookup("Alice").await.as_deref(), Some("He/Him"));
        assert_eq!(c.lookup("bob").await.as_deref(), Some("Any"));
    }

    #[tokio::test]
    async fn unset_unknown_and_unreachable_are_none() {
        let server = MockServer::start().await;
        let c = client(&server).await;
        // 404: user has not set pronouns.
        Mock::given(method("GET"))
            .and(path("/users/nobody"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        // Present, but an id the table doesn't know.
        Mock::given(method("GET"))
            .and(path("/users/weird"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"id": "3", "login": "weird", "pronoun_id": "notathing"}),
            ))
            .mount(&server)
            .await;
        assert_eq!(c.lookup("nobody").await, None);
        assert_eq!(c.lookup("weird").await, None);
        assert_eq!(c.lookup("").await, None);
        // No mock mounted for this path -> wiremock 404s, still None.
        assert_eq!(c.lookup("missing").await, None);
    }
}
