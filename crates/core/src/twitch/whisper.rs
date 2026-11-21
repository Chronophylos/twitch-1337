use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use eyre::{Result, WrapErr as _};
use reqwest::StatusCode;
use serde_json::json;
use tokio::{fs, sync::Mutex};
use tracing::warn;
use twitch_irc::login::LoginCredentials;

use crate::util::{APP_USER_AGENT, http::SendTraced as _, truncate_response};

pub const FIRST_WHISPER_MAX_CHARS: usize = 500;
pub const WHISPER_MAX_CHARS: usize = 10_000;

#[derive(Debug, thiserror::Error)]
pub enum WhisperError {
    #[error("missing user access token for whisper API")]
    MissingToken,
    #[error("failed to obtain Twitch credentials")]
    Credentials(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("whisper message is empty")]
    EmptyMessage,
    #[error("Twitch whisper API returned {status}: {body}")]
    Api { status: StatusCode, body: String },
    #[error("Twitch whisper API request failed")]
    Http(#[from] reqwest::Error),
    #[error("whisper unavailable: {0}")]
    Unavailable(String),
}

impl WhisperError {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable(reason.into())
    }
}

#[async_trait]
pub trait WhisperSender: Send + Sync {
    /// Sends a whisper and returns the actual text submitted to Twitch.
    async fn send_whisper(&self, to_user_id: &str, message: &str) -> Result<String, WhisperError>;
}

pub fn truncate_whisper_message(message: &str, known_recipient: bool) -> String {
    let limit = if known_recipient {
        WHISPER_MAX_CHARS
    } else {
        FIRST_WHISPER_MAX_CHARS
    };
    truncate_response(message, limit)
}

pub struct HelixWhisperSender<L>
where
    L: LoginCredentials,
{
    http: reqwest::Client,
    credentials: L,
    client_id: String,
    from_user_id: String,
    known_recipients: Arc<Mutex<HashSet<String>>>,
    store_path: PathBuf,
}

impl<L> HelixWhisperSender<L>
where
    L: LoginCredentials,
{
    pub async fn new(
        credentials: L,
        client_id: String,
        from_user_id: String,
        data_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let store_path = data_dir.as_ref().join("whisper_recipients.ron");
        let known_recipients = load_known_recipients(&store_path).await;
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()
            .wrap_err("failed to build Twitch whisper HTTP client")?;

        Ok(Self {
            http,
            credentials,
            client_id,
            from_user_id,
            known_recipients: Arc::new(Mutex::new(known_recipients)),
            store_path,
        })
    }

    async fn remember_recipient(&self, to_user_id: &str) {
        // Hold the lock across the save: `atomic_save_ron_async` writes one
        // shared `.ron.tmp` per destination, so two overlapping saves tear it.
        let mut guard = self.known_recipients.lock().await;
        if !guard.insert(to_user_id.to_owned()) {
            return;
        }
        if let Err(error) = save_known_recipients(&self.store_path, &guard).await {
            crate::log_handled!(warn, error, path = %self.store_path.display(),
            "Failed to persist successful whisper recipient");
        }
    }
}

#[async_trait]
impl<L> WhisperSender for HelixWhisperSender<L>
where
    L: LoginCredentials,
    L::Error: std::error::Error + 'static,
{
    async fn send_whisper(&self, to_user_id: &str, message: &str) -> Result<String, WhisperError> {
        let known_recipient = self.known_recipients.lock().await.contains(to_user_id);
        let message = truncate_whisper_message(message, known_recipient);
        if message.trim().is_empty() {
            return Err(WhisperError::EmptyMessage);
        }

        let credentials = self
            .credentials
            .get_credentials()
            .await
            .map_err(|error| WhisperError::Credentials(Box::new(error)))?;
        let token = credentials.token.ok_or(WhisperError::MissingToken)?;

        let response = self
            .http
            .post("https://api.twitch.tv/helix/whispers")
            .query(&[
                ("from_user_id", self.from_user_id.as_str()),
                ("to_user_id", to_user_id),
            ])
            .header("Client-Id", &self.client_id)
            .bearer_auth(token)
            .json(&json!({ "message": message }))
            .send_traced()
            .await?;

        if response.status() == StatusCode::NO_CONTENT {
            self.remember_recipient(to_user_id).await;
            return Ok(message);
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(WhisperError::Api { status, body })
    }
}

/// The store only decides whether a whisper may use the long length limit,
/// so an unreadable or corrupt file costs a few shorter first whispers, never
/// startup.
async fn load_known_recipients(path: &Path) -> HashSet<String> {
    let contents = match fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashSet::new(),
        Err(error) => {
            warn!(?error, path = %path.display(), "Failed to read whisper recipients; starting empty");
            return HashSet::new();
        }
    };
    ron::from_str(&contents).unwrap_or_else(|error| {
        warn!(?error, path = %path.display(), "Failed to parse whisper recipients; starting empty");
        HashSet::new()
    })
}

async fn save_known_recipients(path: &Path, recipients: &HashSet<String>) -> Result<()> {
    crate::util::persist::atomic_save_ron_async(recipients, path)
        .await
        .wrap_err("Failed to save whisper recipients")
}

#[cfg(test)]
mod tests {
    use twitch_irc::login::StaticLoginCredentials;

    use super::*;

    async fn sender(dir: &Path) -> Arc<HelixWhisperSender<StaticLoginCredentials>> {
        crate::install_crypto_provider();
        let creds = StaticLoginCredentials::new("bot".into(), Some("token".into()));
        Arc::new(
            HelixWhisperSender::new(creds, "client".into(), "1".into(), dir)
                .await
                .expect("sender"),
        )
    }

    /// Rounds of overlapping first whispers. After every round the store must
    /// parse (the old unlocked save tore the shared `.ron.tmp`) and hold every
    /// recipient so far (a stale snapshot must never win the last rename).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_recipients_never_tear_the_store() {
        const ROUNDS: usize = 300;
        const WRITERS: usize = 4;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("whisper_recipients.ron");
        let sender = sender(dir.path()).await;
        for round in 0..ROUNDS {
            let tasks: Vec<_> = (0..WRITERS)
                .map(|w| {
                    let sender = sender.clone();
                    let id = format!("{round}-{w}");
                    tokio::spawn(async move { sender.remember_recipient(&id).await })
                })
                .collect();
            for task in tasks {
                task.await.expect("task");
            }
            let stored = fs::read_to_string(&path).await.expect("store written");
            let parsed: HashSet<String> = ron::from_str(&stored)
                .unwrap_or_else(|error| panic!("round {round}: store torn: {error}"));
            assert_eq!(parsed.len(), (round + 1) * WRITERS, "round {round}");
        }
    }

    #[tokio::test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture seeds a corrupt file directly"
    )]
    async fn corrupt_store_does_not_block_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("whisper_recipients.ron"),
            "[\n]   \"1\",\n]",
        )
        .expect("seed");
        let sender = sender(dir.path()).await;
        assert!(sender.known_recipients.lock().await.is_empty());
    }
}
