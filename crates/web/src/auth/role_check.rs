//! Role-gate decision: hidden_admins → broadcaster → Twitch moderated channels.
//!
//! Hidden admins (configured in `twitch.hidden_admins`) short-circuit the
//! helix lookup so a debugging account always retains access. The broadcaster
//! id is checked next as a fast path. Otherwise Twitch is asked, with the
//! user's own token, which channels they moderate: at login with the fresh
//! access token, on each recheck with one minted from the session's refresh
//! token. The bot token cannot answer this: `helix/moderation/moderators`
//! only accepts the broadcaster's own token.

use eyre::eyre;
use secrecy::ExposeSecret as _;

use crate::auth::session::{Session, TwitchRefreshToken};
use crate::state::WebState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    Allow,
    Deny,
}

/// Hidden_admins / broadcaster shortcuts shared by both check variants. Returns
/// `Some(Allow)` iff a shortcut applies; `None` means the helix lookup runs.
pub(crate) fn shortcut(
    user_id: &str,
    broadcaster_id: &str,
    hidden_admins: &[String],
) -> Option<GateOutcome> {
    if hidden_admins.iter().any(|s| s == user_id) || user_id == broadcaster_id {
        Some(GateOutcome::Allow)
    } else {
        None
    }
}

/// Periodic Mod recheck. Trades the session's refresh token for an access
/// token, then asks Twitch whether the user still moderates the channel.
/// Returns the verdict plus the rotated refresh token, if Twitch issued one.
///
/// `Err` means no verdict (Twitch unreachable, refresh token rejected or
/// never stored); the caller must not admit the request.
pub async fn recheck_mod(
    state: &WebState,
    session: &Session,
    hidden_admins: &[String],
) -> eyre::Result<(GateOutcome, Option<TwitchRefreshToken>)> {
    if let Some(o) = shortcut(&session.user_id, &state.broadcaster_id, hidden_admins) {
        return Ok((o, None));
    }
    let refresh_token = session
        .refresh_token
        .as_ref()
        .ok_or_else(|| eyre!("session has no refresh token"))?;
    let tokens = state.oauth.refresh_user_token(refresh_token).await?;
    let is_mod = is_moderator_with_user_token(
        &session.user_id,
        &tokens.access_token,
        &state.broadcaster_id,
        state,
    )
    .await?;
    let rotated = tokens.refresh_token.filter(|t| t != refresh_token);
    let outcome = if is_mod {
        GateOutcome::Allow
    } else {
        GateOutcome::Deny
    };
    Ok((outcome, rotated))
}

/// Variant used during the OAuth callback. Asks Twitch which channels the
/// user moderates (scope `user:read:moderated_channels` on the user token)
/// and checks `broadcaster_id` against that list — the
/// `helix/moderation/moderators` endpoint can't be used here because it
/// requires the bearer to *be* the broadcaster.
pub async fn check_is_mod_with_token(
    state: &WebState,
    user_id: &str,
    user_access_token: &str,
    broadcaster_id: &str,
    hidden_admins: &[String],
) -> eyre::Result<GateOutcome> {
    if let Some(o) = shortcut(user_id, broadcaster_id, hidden_admins) {
        return Ok(o);
    }
    if is_moderator_with_user_token(user_id, user_access_token, broadcaster_id, state).await? {
        Ok(GateOutcome::Allow)
    } else {
        Ok(GateOutcome::Deny)
    }
}

async fn is_moderator_with_user_token(
    user_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    state: &WebState,
) -> eyre::Result<bool> {
    crate::helix::user_moderates_channel(
        &state.oauth.http,
        &state.oauth.helix_api_base,
        state.client_id.expose_secret(),
        access_token,
        user_id,
        broadcaster_id,
        "helix moderated channels (user token)",
    )
    .await
}

/// Allow iff `user_id` appears in `allowlist`.
pub fn check_in_allowlist(user_id: &str, allowlist: &[String]) -> GateOutcome {
    if allowlist.iter().any(|id| id == user_id) {
        GateOutcome::Allow
    } else {
        GateOutcome::Deny
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admins(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    // --- shortcut (pure) ---

    #[test]
    fn shortcut_allows_hidden_admin() {
        assert_eq!(
            shortcut("u1", "broadcaster", &admins(&["u1"])),
            Some(GateOutcome::Allow)
        );
    }

    #[test]
    fn shortcut_allows_broadcaster() {
        assert_eq!(
            shortcut("broadcaster", "broadcaster", &admins(&[])),
            Some(GateOutcome::Allow)
        );
    }

    #[test]
    fn shortcut_returns_none_for_ordinary_user() {
        assert_eq!(shortcut("u2", "broadcaster", &admins(&["u1"])), None);
    }

    // --- check_in_allowlist (pure) ---

    #[test]
    fn allowlist_allows_listed_user() {
        assert_eq!(
            check_in_allowlist("u1", &admins(&["u0", "u1"])),
            GateOutcome::Allow
        );
    }

    #[test]
    fn allowlist_denies_unlisted_and_empty() {
        assert_eq!(
            check_in_allowlist("u9", &admins(&["u1"])),
            GateOutcome::Deny
        );
        assert_eq!(check_in_allowlist("u1", &admins(&[])), GateOutcome::Deny);
    }
}
