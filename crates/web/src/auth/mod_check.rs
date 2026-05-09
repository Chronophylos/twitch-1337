//! Mod-gate decision: hidden_admins → broadcaster → helix moderators.
//!
//! Hidden admins (configured in `[twitch].hidden_admins`) short-circuit the
//! helix lookup so a debugging account always retains access. The broadcaster
//! id is checked next as a fast path. Otherwise we follow the moderator list.

use crate::helix::HelixClient;

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
