//! Speak-only agent mouths. The router owns optional per-agent Helix adapters;
//! an absent mouth returns `NoMouth` and the caller falls back to utility IRC.
//! ponytail: the utility fallback stays at the call site (it needs the live
//! `ctx` privmsg + generic ChatSender) rather than being a generic router arm.

use std::{
    fmt::{self, Display},
    sync::Arc,
};

use async_trait::async_trait;
use eyre::{Result, WrapErr as _};
use serde::Serialize;
use twitch_irc::login::LoginCredentials;

use crate::util::APP_USER_AGENT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Utility,
    Aurora,
    Grok,
}

pub struct SpeakRequest<'a> {
    pub broadcaster_id: &'a str,
    pub channel_login: &'a str,
    pub text: String,
    pub reply_parent_message_id: Option<String>,
}

pub struct SpokenMessage {
    pub message_id: Option<String>,
    pub login: String,
}

pub enum SpeakOutcome {
    Spoken(SpokenMessage),
    NoMouth,
}

#[derive(Debug)]
pub enum SpeakError {
    Empty,
    Credentials(String),
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
    Http(reqwest::Error),
}

impl Display for SpeakError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "chat-send message empty"),
            Self::Credentials(e) => write!(f, "failed to obtain mouth credentials: {e}"),
            Self::Api { status, body } => {
                write!(f, "Twitch chat-send API returned {status}: {body}")
            }
            Self::Http(e) => write!(f, "Twitch chat-send request failed: {e}"),
        }
    }
}
impl std::error::Error for SpeakError {}
impl From<reqwest::Error> for SpeakError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}

#[async_trait]
pub trait Mouth: Send + Sync {
    async fn speak(&self, req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError>;
    fn login(&self) -> &str;
}

pub struct MouthRouter {
    pub aurora: Option<Arc<dyn Mouth>>,
    pub grok: Option<Arc<dyn Mouth>>,
}

impl MouthRouter {
    pub async fn speak(
        &self,
        agent: Agent,
        req: SpeakRequest<'_>,
    ) -> Result<SpeakOutcome, SpeakError> {
        let mouth = match agent {
            Agent::Aurora => self.aurora.as_ref(),
            Agent::Grok => self.grok.as_ref(),
            Agent::Utility => None,
        };
        match mouth {
            Some(m) => Ok(SpeakOutcome::Spoken(m.speak(req).await?)),
            None => Ok(SpeakOutcome::NoMouth),
        }
    }
}

/// Helix `POST /helix/chat/messages` adapter. Mirrors `HelixWhisperSender`
/// (`crates/core/src/twitch/whisper.rs`): same client shape, same
/// `get_credentials().await` → `creds.token` flow, same `Client-Id` +
/// bearer-auth headers. `base_url` is a constructor arg so tests can point
/// it at a local wiremock server; production passes `"https://api.twitch.tv"`.
pub struct HelixMouth<L: LoginCredentials> {
    http: reqwest::Client,
    credentials: L,
    client_id: String,
    user_id: String,
    login: String,
    base_url: String,
}

impl<L: LoginCredentials> HelixMouth<L> {
    pub async fn new(
        credentials: L,
        client_id: String,
        user_id: String,
        login: String,
        base_url: String,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()
            .wrap_err("failed to build Twitch chat-send HTTP client")?;

        Ok(Self {
            http,
            credentials,
            client_id,
            user_id,
            login,
            base_url,
        })
    }

    async fn post_once(
        &self,
        token: &str,
        req: &SpeakRequest<'_>,
    ) -> Result<reqwest::Response, SpeakError> {
        let mut body = serde_json::json!({
            "broadcaster_id": req.broadcaster_id,
            "sender_id": self.user_id,
            "message": req.text,
        });
        if let Some(ref parent) = req.reply_parent_message_id {
            body["reply_parent_message_id"] = serde_json::Value::String(parent.clone());
        }

        Ok(self
            .http
            .post(format!("{}/helix/chat/messages", self.base_url))
            .header("Client-Id", &self.client_id)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?)
    }
}

#[async_trait]
impl<L: LoginCredentials> Mouth for HelixMouth<L> {
    async fn speak(&self, req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError> {
        if req.text.trim().is_empty() {
            return Err(SpeakError::Empty);
        }

        let creds = self
            .credentials
            .get_credentials()
            .await
            .map_err(|error| SpeakError::Credentials(error.to_string()))?;
        let token = creds
            .token
            .ok_or_else(|| SpeakError::Credentials("missing access token".into()))?;

        let mut response = self.post_once(&token, &req).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // 401: force a refresh via get_credentials and retry once.
            let creds = self
                .credentials
                .get_credentials()
                .await
                .map_err(|error| SpeakError::Credentials(error.to_string()))?;
            let token = creds
                .token
                .ok_or_else(|| SpeakError::Credentials("missing access token".into()))?;
            response = self.post_once(&token, &req).await?;
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(SpeakError::Api { status, body });
        }

        let parsed: SendChatResponse = response.json().await?;
        let sent = parsed.data.into_iter().next();
        let message_id = sent
            .filter(|datum| datum.is_sent)
            .and_then(|datum| datum.message_id);
        Ok(SpokenMessage {
            message_id,
            login: self.login.clone(),
        })
    }

    fn login(&self) -> &str {
        &self.login
    }
}

#[derive(serde::Deserialize)]
struct SendChatResponse {
    data: Vec<SendChatDatum>,
}

#[derive(serde::Deserialize)]
struct SendChatDatum {
    message_id: Option<String>,
    #[serde(default)]
    is_sent: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct FakeMouth {
        login: String,
    }
    #[async_trait::async_trait]
    impl Mouth for FakeMouth {
        async fn speak(&self, _req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError> {
            Ok(SpokenMessage {
                message_id: Some("id1".into()),
                login: self.login.clone(),
            })
        }
        fn login(&self) -> &str {
            &self.login
        }
    }

    fn req() -> SpeakRequest<'static> {
        SpeakRequest {
            broadcaster_id: "1",
            channel_login: "chan",
            text: "hi".into(),
            reply_parent_message_id: None,
        }
    }

    #[tokio::test]
    async fn routes_to_configured_mouth() {
        let router = MouthRouter {
            aurora: Some(Arc::new(FakeMouth {
                login: "aurora_bot".into(),
            })),
            grok: None,
        };
        let out = router.speak(Agent::Aurora, req()).await.unwrap();
        assert!(
            matches!(out, SpeakOutcome::Spoken(SpokenMessage { login, .. }) if login == "aurora_bot")
        );
    }

    #[tokio::test]
    async fn absent_mouth_yields_no_mouth() {
        let router = MouthRouter {
            aurora: None,
            grok: None,
        };
        assert!(matches!(
            router.speak(Agent::Grok, req()).await.unwrap(),
            SpeakOutcome::NoMouth
        ));
        assert!(matches!(
            router.speak(Agent::Utility, req()).await.unwrap(),
            SpeakOutcome::NoMouth
        ));
    }
}
