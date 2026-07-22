//! Speak-only agent mouths. The router owns optional per-agent Helix adapters;
//! an absent mouth returns `NoMouth` and the caller falls back to utility IRC.
//! ponytail: the utility fallback stays at the call site (it needs the live
//! `ctx` privmsg + generic ChatSender) rather than being a generic router arm.

use std::{
    fmt::{self, Display},
    sync::Arc,
};

use async_trait::async_trait;
use serde::Serialize;

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
