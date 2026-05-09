//! Minimal Twitch helix client (broadcaster id, moderator list, user lookup).
//!
//! Mirrors the AviationClient pattern in core for testability — boxed behind
//! a trait so route tests can inject fakes. The real impl follows pagination
//! cursors when listing moderators.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr as _};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;

#[async_trait]
pub trait HelixClient: Send + Sync {
    async fn fetch_user_by_id(&self, user_id: &str) -> Result<Option<HelixUser>>;
    async fn fetch_user_by_login(&self, login: &str) -> Result<Option<HelixUser>>;
    /// Follows pagination until exhausted; returns true if `user_id` is in the moderator list.
    async fn is_moderator(&self, broadcaster_id: &str, user_id: &str) -> Result<bool>;
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelixUser {
    pub id: String,
    pub login: String,
    pub display_name: String,
}

#[async_trait]
pub trait AccessTokenProvider: Send + Sync {
    async fn current_access_token(&self) -> Result<String>;
}

pub struct ReqwestHelixClient {
    pub http: reqwest::Client,
    pub client_id: SecretString,
    pub access_token_provider: Arc<dyn AccessTokenProvider>,
    /// `https://api.twitch.tv` in production. Tests inject a wiremock URI.
    pub helix_base: String,
}

impl ReqwestHelixClient {
    pub fn new(
        http: reqwest::Client,
        client_id: SecretString,
        provider: Arc<dyn AccessTokenProvider>,
    ) -> Self {
        Self::with_base(http, client_id, provider, "https://api.twitch.tv".into())
    }

    pub fn with_base(
        http: reqwest::Client,
        client_id: SecretString,
        provider: Arc<dyn AccessTokenProvider>,
        base: String,
    ) -> Self {
        Self {
            http,
            client_id,
            access_token_provider: provider,
            helix_base: base,
        }
    }

    async fn fetch_user(&self, query: &[(&str, &str)]) -> Result<Option<HelixUser>> {
        #[derive(Deserialize)]
        struct UserResp {
            data: Vec<HelixUser>,
        }
        let mut url = url::Url::parse(&format!("{}/helix/users", self.helix_base))?;
        for (k, v) in query {
            url.query_pairs_mut().append_pair(k, v);
        }
        let token = self.access_token_provider.current_access_token().await?;
        let resp: UserResp = self
            .http
            .get(url)
            .bearer_auth(&token)
            .header("Client-Id", self.client_id.expose_secret())
            .send()
            .await?
            .error_for_status()
            .wrap_err("helix users")?
            .json()
            .await?;
        Ok(resp.data.into_iter().next())
    }
}

#[async_trait]
impl HelixClient for ReqwestHelixClient {
    async fn fetch_user_by_id(&self, user_id: &str) -> Result<Option<HelixUser>> {
        self.fetch_user(&[("id", user_id)]).await
    }

    async fn fetch_user_by_login(&self, login: &str) -> Result<Option<HelixUser>> {
        self.fetch_user(&[("login", login)]).await
    }

    async fn is_moderator(&self, broadcaster_id: &str, user_id: &str) -> Result<bool> {
        #[derive(Deserialize)]
        struct ModEntry {
            user_id: String,
        }
        #[derive(Deserialize)]
        struct Pagination {
            cursor: Option<String>,
        }
        #[derive(Deserialize)]
        struct ModResp {
            data: Vec<ModEntry>,
            #[serde(default)]
            pagination: Option<Pagination>,
        }

        let mut cursor: Option<String> = None;
        loop {
            let mut url =
                url::Url::parse(&format!("{}/helix/moderation/moderators", self.helix_base))?;
            url.query_pairs_mut()
                .append_pair("broadcaster_id", broadcaster_id)
                .append_pair("first", "100");
            if let Some(c) = &cursor {
                url.query_pairs_mut().append_pair("after", c);
            }
            let token = self.access_token_provider.current_access_token().await?;
            let resp: ModResp = self
                .http
                .get(url)
                .bearer_auth(&token)
                .header("Client-Id", self.client_id.expose_secret())
                .send()
                .await?
                .error_for_status()
                .wrap_err("helix moderators")?
                .json()
                .await?;
            if resp.data.iter().any(|m| m.user_id == user_id) {
                return Ok(true);
            }
            match resp.pagination.and_then(|p| p.cursor) {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => return Ok(false),
            }
        }
    }
}
