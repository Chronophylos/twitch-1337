//! 7TV REST fetch for emote-tracker channel-set ingestion.

use eyre::{Result, WrapErr as _, bail};
use serde::Deserialize;

use super::reconcile::DesiredEmote;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSetSnapshot {
    pub set_id: String,
    pub emotes: Vec<DesiredEmote>,
}

#[derive(Debug, Deserialize)]
struct SevenTvUserResponse {
    emote_set: Option<SevenTvEmoteSetResponse>,
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
    fn from_emote_set(set: SevenTvEmoteSetResponse) -> Self {
        Self {
            set_id: set.id,
            emotes: set
                .emotes
                .into_iter()
                .map(|emote| DesiredEmote {
                    emote_id: emote.id,
                    name: emote.name,
                })
                .collect(),
        }
    }

    #[cfg(test)]
    fn from_user_json(json: &str) -> Result<Self> {
        let user: SevenTvUserResponse = serde_json::from_str(json)?;
        let set = user
            .emote_set
            .ok_or_else(|| eyre::eyre!("response missing emote_set"))?;
        Ok(Self::from_emote_set(set))
    }
}

/// Fetch the channel's active 7TV emote set with per-emote ids.
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
    let set = user
        .emote_set
        .ok_or_else(|| eyre::eyre!("response missing emote_set"))?;
    Ok(ChannelSetSnapshot::from_emote_set(set))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_user_set_id_and_emote_ids() {
        let json = r#"{
      "emote_set": {
        "id": "set123",
        "emotes": [
          {"id": "e1", "name": "Kappa"},
          {"id": "e2", "name": "Pog"}
        ]
      }
    }"#;
        let snap = ChannelSetSnapshot::from_user_json(json).unwrap();
        assert_eq!(snap.set_id, "set123");
        assert_eq!(snap.emotes.len(), 2);
        assert_eq!(snap.emotes[0].emote_id, "e1");
    }
}
