//! Startup construction of the AI mouths router + echo guard from
//! `[twitch.mouths.*]` config. Each configured mouth gets its own
//! `RefreshingLoginCredentials` (separate token file under `data_dir`) and
//! its Twitch user id resolved via Helix Get Users. A mouth that fails to
//! resolve or build is skipped (logged, not fatal) — its login stays
//! registered with the echo guard so echoes are still filtered even when
//! the user id is unknown, but `MouthRouter` won't have an adapter to speak
//! through for it.

use std::{path::Path, sync::Arc};

use eyre::{Result, WrapErr as _};
use secrecy::ExposeSecret as _;
use tracing::warn;
use twitch_irc::login::{LoginCredentials, RefreshingLoginCredentials};

use crate::{
    AuthenticatedLoginCredentials,
    config::{Configuration, MouthConfig},
    twitch::{
        echo::{EchoGuard, MouthIdentities},
        mouth::{HelixMouth, Mouth, MouthRouter},
        token_storage::FileBasedTokenStorage,
    },
};

/// Build the mouth router + echo guard from bootstrap config. Never fails:
/// a missing or broken mouth is skipped and logged, leaving that agent
/// without a Helix adapter (the reply path falls back to utility IRC via
/// `SpeakOutcome::NoMouth`).
pub async fn build_mouths(
    config: &Configuration,
    data_dir: &Path,
) -> (Arc<MouthRouter>, Arc<EchoGuard>) {
    let mut identities = MouthIdentities::default();
    let aurora = build_one(
        config,
        data_dir,
        "token-aurora.ron",
        config.twitch.mouths.aurora.as_ref(),
        &mut identities,
    )
    .await;
    let grok = build_one(
        config,
        data_dir,
        "token-grok.ron",
        config.twitch.mouths.grok.as_ref(),
        &mut identities,
    )
    .await;
    (
        Arc::new(MouthRouter { aurora, grok }),
        Arc::new(EchoGuard::new(identities)),
    )
}

async fn build_one(
    config: &Configuration,
    data_dir: &Path,
    file: &str,
    cfg: Option<&MouthConfig>,
    identities: &mut MouthIdentities,
) -> Option<Arc<dyn Mouth>> {
    let cfg = cfg?;
    // Store side must lowercase to match `EchoGuard::is_mouth`'s compare side
    // (`sender_login.to_ascii_lowercase()`), or identity matching silently
    // breaks whenever config casing differs from what Twitch sends back.
    identities.logins.insert(cfg.username.to_ascii_lowercase());

    let creds: AuthenticatedLoginCredentials = RefreshingLoginCredentials::init_with_username(
        Some(cfg.username.clone()),
        config.twitch.client_id.expose_secret().to_string(),
        config.twitch.client_secret.expose_secret().to_string(),
        FileBasedTokenStorage::with_path(data_dir.join(file), cfg.refresh_token.clone()),
    );

    let user_id = match resolve_user_id(
        &creds,
        config.twitch.client_id.expose_secret(),
        &cfg.username,
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            warn!(?error, mouth = %cfg.username, "mouth unavailable: user-id resolve failed");
            return None;
        }
    };
    identities.user_ids.insert(user_id.clone());

    match HelixMouth::new(
        creds,
        config.twitch.client_id.expose_secret().to_string(),
        user_id,
        cfg.username.clone(),
        "https://api.twitch.tv".to_string(),
    )
    .await
    {
        Ok(m) => Some(Arc::new(m) as Arc<dyn Mouth>),
        Err(error) => {
            warn!(?error, mouth = %cfg.username, "mouth unavailable: adapter build failed");
            None
        }
    }
}

#[derive(serde::Deserialize)]
struct GetUsersResponse {
    data: Vec<GetUsersDatum>,
}

#[derive(serde::Deserialize)]
struct GetUsersDatum {
    id: String,
}

/// Resolve a Twitch user id from a login via Helix Get Users, using the
/// mouth's own (refreshing) credentials for the bearer token. Get Users
/// needs a valid token but no extra scope.
async fn resolve_user_id<L: LoginCredentials>(
    credentials: &L,
    client_id: &str,
    username: &str,
) -> Result<String> {
    let creds = credentials
        .get_credentials()
        .await
        .map_err(|error| eyre::eyre!("get_credentials: {error}"))?;
    let token = creds
        .token
        .ok_or_else(|| eyre::eyre!("missing access token"))?;

    let http = reqwest::Client::builder()
        .user_agent(crate::util::APP_USER_AGENT)
        .build()
        .wrap_err("failed to build Twitch Get Users HTTP client")?;

    let response = http
        .get("https://api.twitch.tv/helix/users")
        .query(&[("login", username)])
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await
        .wrap_err("Get Users request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        eyre::bail!("Get Users returned {status}: {body}");
    }

    let parsed: GetUsersResponse = response
        .json()
        .await
        .wrap_err("failed to parse Get Users response")?;
    parsed
        .data
        .into_iter()
        .next()
        .map(|u| u.id)
        .ok_or_else(|| eyre::eyre!("no user found for login `{username}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_mouth_section_yields_empty_router_and_guard() {
        let cfg = Configuration::test_default(); // no mouths
        let (router, guard) = build_mouths(&cfg, Path::new("/tmp")).await;
        assert!(router.aurora.is_none() && router.grok.is_none());
        assert!(guard.mouth_logins().is_empty());
    }
}
