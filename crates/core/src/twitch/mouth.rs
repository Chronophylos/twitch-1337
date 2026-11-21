//! Speak-only agent mouths. The router owns optional per-agent Helix adapters;
//! an absent mouth returns `NoMouth` and the caller falls back to utility IRC.
//! ponytail: the utility fallback stays at the call site (it needs the live
//! `ctx` privmsg + generic ChatSender) rather than being a generic router arm.
//! Outbound text is sanitized in [`MouthRouter::speak`] — mouths speak over
//! Helix, so they never touch `ChatSender` and would otherwise skip the
//! control-char strip. Length clamping for agent replies happens before
//! `speak`, so the router must not apply the 480-byte IRC ceiling.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use eyre::{Result, WrapErr as _};
use serde::Serialize;
use twitch_irc::login::LoginCredentials;

use crate::util::APP_USER_AGENT;
use crate::util::http::{SendTraced as _, TracedResponse};

/// Ceiling on any single Helix call made by a mouth. Without it a hung
/// connection blocks the caller forever.
pub(crate) const HELIX_TIMEOUT: Duration = Duration::from_secs(10);

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

#[derive(Debug)]
pub struct SpokenMessage {
    pub message_id: Option<String>,
    pub login: String,
}

pub enum SpeakOutcome {
    Spoken(SpokenMessage),
    NoMouth,
}

#[derive(Debug, thiserror::Error)]
pub enum SpeakError {
    #[error("chat-send message empty")]
    Empty,
    #[error("failed to obtain mouth credentials")]
    Credentials(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("mouth credentials carry no access token")]
    MissingToken,
    #[error("Twitch chat-send API returned {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
    /// Helix accepted the request (HTTP 200) but did not deliver the message:
    /// `is_sent: false`, typically an AutoMod hold, a channel restriction, or
    /// the mouth account being banned/timed out. Chat never saw it, so this is
    /// a failure — callers must not record history or transcript rows.
    #[error(
        "Twitch dropped the chat message: {} ({})",
        message.as_deref().unwrap_or("no reason given"),
        code.as_deref().unwrap_or("no code")
    )]
    Dropped {
        code: Option<String>,
        message: Option<String>,
    },
    #[error("Twitch chat-send request failed")]
    Http(#[from] reqwest::Error),
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
        mut req: SpeakRequest<'_>,
    ) -> Result<SpeakOutcome, SpeakError> {
        // Helix rejects anything over 500 chars with a 422. Agent replies are
        // already character-clamped to 490 before they reach the router; only
        // the control-char strip runs here. The 480-byte IRC clamp would cut
        // a 490-character ASCII line.
        req.text = crate::twitch::chat_sender::collapse_control_and_whitespace(&req.text);
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

    /// Speaking login for `agent` when that Mouth adapter exists.
    /// `Agent::Utility` and a missing adapter are `None` — callers fall back
    /// to the utility bot login for prompt copy, matching `SpeakOutcome::NoMouth`.
    pub fn login_for(&self, agent: Agent) -> Option<&str> {
        match agent {
            Agent::Aurora => self.aurora.as_ref().map(|m| m.login()),
            Agent::Grok => self.grok.as_ref().map(|m| m.login()),
            Agent::Utility => None,
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
        // Commands run inline in the dispatcher loop: an untimed POST here
        // would wedge every command, not just this one.
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(HELIX_TIMEOUT)
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
    ) -> Result<TracedResponse, SpeakError> {
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
            .send_traced()
            .await?)
    }
}

#[async_trait]
impl<L> Mouth for HelixMouth<L>
where
    L: LoginCredentials,
    L::Error: std::error::Error + 'static,
{
    async fn speak(&self, req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError> {
        if req.text.trim().is_empty() {
            return Err(SpeakError::Empty);
        }

        let creds = self
            .credentials
            .get_credentials()
            .await
            .map_err(|error| SpeakError::Credentials(Box::new(error)))?;
        let token = creds.token.ok_or(SpeakError::MissingToken)?;

        let mut response = self.post_once(&token, &req).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // 401: force a refresh via get_credentials and retry once.
            let creds = self
                .credentials
                .get_credentials()
                .await
                .map_err(|error| SpeakError::Credentials(Box::new(error)))?;
            let token = creds.token.ok_or(SpeakError::MissingToken)?;
            response = self.post_once(&token, &req).await?;
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(SpeakError::Api { status, body });
        }

        let parsed: SendChatResponse = response.json().await?;
        let Some(datum) = parsed.data.into_iter().next() else {
            return Err(SpeakError::Dropped {
                code: None,
                message: Some("empty data array".into()),
            });
        };
        if !datum.is_sent {
            let (code, message) = datum
                .drop_reason
                .map_or((None, None), |r| (Some(r.code), r.message));
            return Err(SpeakError::Dropped { code, message });
        }
        Ok(SpokenMessage {
            message_id: datum.message_id,
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
    #[serde(default)]
    drop_reason: Option<DropReason>,
}

#[derive(serde::Deserialize)]
struct DropReason {
    code: String,
    message: Option<String>,
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

    fn fake(login: &str) -> Option<Arc<dyn Mouth>> {
        Some(Arc::new(FakeMouth {
            login: login.into(),
        }))
    }

    fn req() -> SpeakRequest<'static> {
        SpeakRequest {
            broadcaster_id: "1",
            channel_login: "chan",
            text: "hi".into(),
            reply_parent_message_id: None,
        }
    }

    #[test]
    fn login_for_returns_configured_adapter_login() {
        let router = MouthRouter {
            aurora: fake("aurora_bot"),
            grok: fake("grok_bot"),
        };
        assert_eq!(router.login_for(Agent::Aurora), Some("aurora_bot"));
        assert_eq!(router.login_for(Agent::Grok), Some("grok_bot"));
    }

    #[test]
    fn login_for_absent_or_utility_is_none() {
        let router = MouthRouter {
            aurora: fake("aurora_bot"),
            grok: None,
        };
        assert_eq!(router.login_for(Agent::Grok), None);
        assert_eq!(router.login_for(Agent::Utility), None);
    }

    #[tokio::test]
    async fn routes_to_configured_mouth() {
        let router = MouthRouter {
            aurora: fake("aurora_bot"),
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

    /// Agent replies are character-clamped to 490 before they reach the
    /// router. The 480-byte IRC clamp must not run here: a 490-character
    /// ASCII line is over 480 bytes and would be cut.
    #[tokio::test]
    async fn agent_line_is_not_byte_clamped() {
        use std::sync::Mutex;

        struct Recorder(Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Mouth for Recorder {
            async fn speak(&self, req: SpeakRequest<'_>) -> Result<SpokenMessage, SpeakError> {
                self.0.lock().unwrap().push(req.text);
                Ok(SpokenMessage {
                    message_id: None,
                    login: "rec".into(),
                })
            }
            fn login(&self) -> &str {
                "rec"
            }
        }

        let rec = Arc::new(Recorder(Mutex::new(Vec::new())));
        let router = MouthRouter {
            aurora: Some(rec.clone()),
            grok: None,
        };
        let long = "a".repeat(490);
        router
            .speak(
                Agent::Aurora,
                SpeakRequest {
                    broadcaster_id: "1",
                    channel_login: "chan",
                    text: format!("{long}\r\ntail"),
                    reply_parent_message_id: None,
                },
            )
            .await
            .unwrap();

        let sent = rec.0.lock().unwrap()[0].clone();
        assert_eq!(
            sent.chars().count(),
            495, // 490 + space (collapsed \r\n) + "tail"
            "490-char ASCII must not be cut by the 480-byte clamp, got {}",
            sent.chars().count(),
        );
        assert!(
            !sent.contains('\r') && !sent.contains('\n'),
            "control chars must be stripped: {sent:?}",
        );
    }
}
