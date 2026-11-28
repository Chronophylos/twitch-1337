use async_trait::async_trait;
use color_eyre::eyre::{self, Result, WrapErr as _};
use jiff::Timestamp;
use secrecy::{ExposeSecret, SecretString};
use tokio::fs;
use tracing::{debug, instrument, warn};
use twitch_irc::login::{TokenStorage, UserAccessToken};

use crate::util::get_data_dir;

/// Token storage implementation that persists tokens to disk.
///
/// Falls back to initial refresh token from config on first load if no token file exists.
#[derive(Debug, Clone)]
pub struct FileBasedTokenStorage {
    path: std::path::PathBuf,
    initial_refresh_token: SecretString,
}

impl FileBasedTokenStorage {
    pub fn new(initial_refresh_token: SecretString) -> Self {
        Self::with_path(get_data_dir().join("token.ron"), initial_refresh_token)
    }

    pub fn with_path(path: std::path::PathBuf, initial_refresh_token: SecretString) -> Self {
        Self {
            path,
            initial_refresh_token,
        }
    }

    fn fresh_token_from_config(&self) -> UserAccessToken {
        let now = Timestamp::now();
        UserAccessToken {
            access_token: String::new(),
            refresh_token: self.initial_refresh_token.expose_secret().to_string(),
            created_at: now,
            expires_at: Some(now),
        }
    }
}

#[async_trait]
impl TokenStorage for FileBasedTokenStorage {
    type LoadError = eyre::Report;
    type UpdateError = eyre::Report;

    #[instrument(skip_all)]
    async fn load_token(&mut self) -> Result<UserAccessToken, Self::LoadError> {
        match fs::read_to_string(&self.path).await {
            Ok(contents) => {
                debug!(
                    path = %self.path.display(),
                    "Loading user access token from file"
                );
                // A torn or empty file (crash mid-save) must not block login.
                // The stored token is only used when it matches the config one,
                // so falling back loses nothing that would have been used.
                let token: UserAccessToken = match ron::from_str(&contents) {
                    Ok(token) => token,
                    Err(error) => {
                        warn!(
                            ?error,
                            path = %self.path.display(),
                            "Token file is unreadable, using refresh token from configuration"
                        );
                        return Ok(self.fresh_token_from_config());
                    }
                };
                let config_token = self.initial_refresh_token.expose_secret();
                if token.refresh_token != config_token {
                    warn!("Refresh token in config differs from stored token; using config token");
                    return Ok(self.fresh_token_from_config());
                }
                Ok(token)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!("Token file not found, using refresh token from configuration");
                Ok(self.fresh_token_from_config())
            }
            Err(e) => Err(eyre::Report::from(e).wrap_err("Failed to read token file")),
        }
    }

    #[instrument(skip_all)]
    async fn update_token(&mut self, token: &UserAccessToken) -> Result<(), Self::UpdateError> {
        debug!(path = %self.path.display(), "Updating token in file");
        crate::util::persist::atomic_save_ron_private_async(token, &self.path)
            .await
            .wrap_err("Failed to save token")
    }
}

#[cfg(test)]
mod tests {
    use twitch_irc::login::UserAccessToken;

    /// The token file holds a live access + refresh token, so it must be
    /// owner-only, also when an older build left it world-readable.
    #[cfg(unix)]
    #[tokio::test]
    async fn update_token_writes_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt as _;

        use jiff::Timestamp;
        use twitch_irc::login::TokenStorage as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-aurora.ron");
        #[expect(clippy::disallowed_methods, reason = "test seeds a 0644 token file")]
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut storage = super::FileBasedTokenStorage::with_path(path.clone(), "ref".into());
        let token = UserAccessToken {
            access_token: "acc".to_owned(),
            refresh_token: "ref".to_owned(),
            created_at: Timestamp::UNIX_EPOCH,
            expires_at: None,
        };
        storage.update_token(&token).await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        assert_eq!(storage.load_token().await.unwrap().access_token, "acc");
    }

    #[tokio::test]
    async fn unparsable_token_file_falls_back_to_the_config_token() {
        use twitch_irc::login::TokenStorage as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.ron");
        #[expect(clippy::disallowed_methods, reason = "test seeds a torn token file")]
        std::fs::write(&path, "").unwrap();

        let mut storage = super::FileBasedTokenStorage::with_path(path, "ref".into());
        let token = storage.load_token().await.unwrap();
        assert_eq!(token.refresh_token, "ref");
        assert!(token.access_token.is_empty());
    }

    /// `token.ron` written by the chrono-era build must keep loading, or the
    /// bot falls back to the config refresh token on every start.
    #[test]
    fn chrono_era_token_file_loads() {
        let t: UserAccessToken = ron::from_str(
            r#"(access_token: "acc", refresh_token: "ref",
                created_at: "2026-07-19T12:00:00.123456789Z",
                expires_at: Some("2026-07-19T16:00:00Z"))"#,
        )
        .expect("old token file must load");
        assert_eq!(t.expires_at, Some("2026-07-19T16:00:00Z".parse().unwrap()));
    }
}
