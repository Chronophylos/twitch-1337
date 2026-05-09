//! Mod-gate decision: hidden_admins → broadcaster → helix moderators.
//!
//! Hidden admins (configured in `[twitch].hidden_admins`) short-circuit the
//! helix lookup so a debugging account always retains access. The broadcaster
//! id is checked next as a fast path. Otherwise we follow the moderator list.

use eyre::WrapErr as _;
use secrecy::ExposeSecret as _;
use serde::Deserialize;

use crate::helix::HelixClient;
use crate::state::WebState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModCheckOutcome {
    Allow,
    Deny,
}

pub async fn check_is_mod(
    helix: &dyn HelixClient,
    user_id: &str,
    broadcaster_id: &str,
    hidden_admins: &[String],
) -> eyre::Result<ModCheckOutcome> {
    if hidden_admins.iter().any(|s| s == user_id) {
        return Ok(ModCheckOutcome::Allow);
    }
    if user_id == broadcaster_id {
        return Ok(ModCheckOutcome::Allow);
    }
    if helix.is_moderator(broadcaster_id, user_id).await? {
        return Ok(ModCheckOutcome::Allow);
    }
    Ok(ModCheckOutcome::Deny)
}

/// Variant used during the OAuth callback. The user's own access token has
/// `moderation:read` (granted via the requested scope), so the helix call
/// works regardless of the bot token's scopes. Hidden_admins + broadcaster
/// short-circuits stay identical.
pub async fn check_is_mod_with_token(
    state: &WebState,
    user_id: &str,
    user_access_token: &str,
    broadcaster_id: &str,
    hidden_admins: &[String],
) -> eyre::Result<ModCheckOutcome> {
    if hidden_admins.iter().any(|s| s == user_id) {
        return Ok(ModCheckOutcome::Allow);
    }
    if user_id == broadcaster_id {
        return Ok(ModCheckOutcome::Allow);
    }
    let is_mod =
        is_moderator_with_user_token(user_id, user_access_token, broadcaster_id, state).await?;
    if is_mod {
        Ok(ModCheckOutcome::Allow)
    } else {
        Ok(ModCheckOutcome::Deny)
    }
}

async fn is_moderator_with_user_token(
    user_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    state: &WebState,
) -> eyre::Result<bool> {
    #[derive(Deserialize)]
    struct Mod {
        user_id: String,
    }
    #[derive(Deserialize)]
    struct Pagination {
        cursor: Option<String>,
    }
    #[derive(Deserialize)]
    struct Resp {
        data: Vec<Mod>,
        pagination: Option<Pagination>,
    }

    let client_id = state.client_id.expose_secret().to_owned();
    let mut cursor: Option<String> = None;
    loop {
        let mut url = url::Url::parse("https://api.twitch.tv/helix/moderation/moderators")?;
        url.query_pairs_mut()
            .append_pair("broadcaster_id", broadcaster_id)
            .append_pair("first", "100");
        if let Some(c) = &cursor {
            url.query_pairs_mut().append_pair("after", c);
        }
        let resp: Resp = state
            .oauth
            .http
            .get(url)
            .bearer_auth(access_token)
            .header("Client-Id", &client_id)
            .send()
            .await?
            .error_for_status()
            .wrap_err("helix moderators (user token)")?
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
