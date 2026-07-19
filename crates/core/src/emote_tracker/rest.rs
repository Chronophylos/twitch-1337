//! 7TV REST fetch for emote-tracker channel-set ingestion.

use eyre::{Result, WrapErr as _, bail};
use serde::Deserialize;

use super::reconcile::DesiredEmote;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSetSnapshot {
    /// 7TV user id (for `user.update` EventAPI subscription).
    pub user_id: String,
    pub set_id: String,
    pub emotes: Vec<DesiredEmote>,
}

#[derive(Debug, Deserialize)]
struct SevenTvUserResponse {
    /// Nested 7TV user document (`/v3/users/twitch/{id}` wraps the connection).
    user: Option<SevenTvUserInner>,
    emote_set: Option<SevenTvEmoteSetResponse>,
}

#[derive(Debug, Deserialize)]
struct SevenTvUserInner {
    id: String,
}

#[derive(Debug, Deserialize)]
struct SevenTvEmoteSetResponse {
    id: String,
    #[serde(default)]
    emotes: Vec<SevenTvEmoteResponse>,
}

#[derive(Debug, Deserialize)]
struct SevenTvEmoteResponse {
    id: String,
    name: String,
}

impl ChannelSetSnapshot {
    fn from_response(user: SevenTvUserResponse) -> Result<Self> {
        let user_id = user
            .user
            .map(|u| u.id)
            .ok_or_else(|| eyre::eyre!("response missing user.id"))?;
        let set = user
            .emote_set
            .ok_or_else(|| eyre::eyre!("response missing emote_set"))?;
        Ok(Self {
            user_id,
            set_id: set.id,
            emotes: set
                .emotes
                .into_iter()
                .map(|emote| DesiredEmote {
                    emote_id: emote.id,
                    name: emote.name,
                })
                .collect(),
        })
    }

    #[cfg(test)]
    fn from_user_json(json: &str) -> Result<Self> {
        let user: SevenTvUserResponse = serde_json::from_str(json)?;
        Self::from_response(user)
    }
}

/// Fetch the channel's active 7TV emote set with per-emote ids and 7TV user id.
pub async fn fetch_channel_set(
    http: &reqwest::Client,
    base_url: &str,
    twitch_channel_id: &str,
) -> Result<ChannelSetSnapshot> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/users/twitch/{twitch_channel_id}");
    let response = http
        .get(&url)
        .send()
        .await
        .wrap_err_with(|| format!("Failed to send 7TV request to {url}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("7TV request failed with status {status}: {body}");
    }

    let user: SevenTvUserResponse = response
        .json()
        .await
        .wrap_err_with(|| format!("Failed to parse 7TV response from {url}"))?;
    ChannelSetSnapshot::from_response(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_user_set_id_user_id_and_emote_ids() {
        let json = r#"{
      "id": "784681545",
      "platform": "TWITCH",
      "user": { "id": "01USERID", "username": "streamer" },
      "emote_set": {
        "id": "set123",
        "emotes": [
          {"id": "e1", "name": "Kappa"},
          {"id": "e2", "name": "Pog"}
        ]
      }
    }"#;
        let snap = ChannelSetSnapshot::from_user_json(json).unwrap();
        assert_eq!(snap.user_id, "01USERID");
        assert_eq!(snap.set_id, "set123");
        assert_eq!(snap.emotes.len(), 2);
        assert_eq!(snap.emotes[0].emote_id, "e1");
    }
}
