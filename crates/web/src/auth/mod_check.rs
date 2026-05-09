//! Mod-gate decision: hidden_admins → broadcaster → helix moderators.
//!
//! Hidden admins (configured in `[twitch].hidden_admins`) short-circuit the
//! helix lookup so a debugging account always retains access. The broadcaster
//! id is checked next as a fast path. Otherwise we follow the moderator list.

use secrecy::ExposeSecret as _;

use crate::helix::HelixClient;
use crate::state::WebState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModCheckOutcome {
    Allow,
    Deny,
}

/// Hidden_admins / broadcaster shortcuts shared by both check variants. Returns
/// `Some(Allow)` iff a shortcut applies; `None` means the helix lookup runs.
fn shortcut(
    user_id: &str,
    broadcaster_id: &str,
    hidden_admins: &[String],
) -> Option<ModCheckOutcome> {
    if hidden_admins.iter().any(|s| s == user_id) || user_id == broadcaster_id {
        Some(ModCheckOutcome::Allow)
    } else {
        None
    }
}

pub async fn check_is_mod(
    helix: &dyn HelixClient,
    user_id: &str,
    broadcaster_id: &str,
    hidden_admins: &[String],
) -> eyre::Result<ModCheckOutcome> {
    if let Some(o) = shortcut(user_id, broadcaster_id, hidden_admins) {
        return Ok(o);
    }
    if helix.is_moderator(broadcaster_id, user_id).await? {
        Ok(ModCheckOutcome::Allow)
    } else {
        Ok(ModCheckOutcome::Deny)
    }
}

/// Variant used during the OAuth callback. The user's own access token has
/// `moderation:read` (granted via the requested scope), so the helix call
/// works regardless of the bot token's scopes.
pub async fn check_is_mod_with_token(
    state: &WebState,
    user_id: &str,
    user_access_token: &str,
    broadcaster_id: &str,
    hidden_admins: &[String],
) -> eyre::Result<ModCheckOutcome> {
    if let Some(o) = shortcut(user_id, broadcaster_id, hidden_admins) {
        return Ok(o);
    }
    if is_moderator_with_user_token(user_id, user_access_token, broadcaster_id, state).await? {
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
    crate::helix::helix_moderator_check(
        &state.oauth.http,
        "https://api.twitch.tv",
        state.client_id.expose_secret(),
        access_token,
        broadcaster_id,
        user_id,
        "helix moderators (user token)",
    )
    .await
}
