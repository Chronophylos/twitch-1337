//! 7TV REST fetch for emote-tracker channel-set ingestion.

use eyre::{Result, WrapErr as _, bail};
use serde::Deserialize;

use super::reconcile::DesiredEmote;
use crate::util::http::{SendTraced as _, read_capped};

/// Body cap. 7TV inlines each emote's full document and host file list
/// (~2.4 KiB per emote; a 969-emote set is 2.3 MiB live). 16 MiB covers sets
/// several times the usual 1000-slot ceiling.
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

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
                    actor_id: None,
                })
                .collect(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalCatalogEmote {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct GlobalSetResponse {
    #[serde(default)]
    emotes: Vec<SevenTvEmoteResponse>,
}

/// Fetch global 7TV ids/names for usage matching. These never become Active
/// Catalog membership and have no add Set Event.
pub async fn fetch_global_set(
    http: &reqwest::Client,
    base_url: &str,
) -> Result<Vec<GlobalCatalogEmote>> {
    let url = format!("{}/emote-sets/global", base_url.trim_end_matches('/'));
    let response = http
        .get(&url)
        .send_traced()
        .await
        .wrap_err_with(|| format!("Failed to send 7TV request to {url}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = read_capped(response, MAX_BODY_BYTES)
            .await
            .map(String::from_utf8_lossy_owned)
            .unwrap_or_default();
        bail!("7TV request failed with status {status}: {body}");
    }
    let body = read_capped(response, MAX_BODY_BYTES)
        .await
        .wrap_err_with(|| format!("Failed to read 7TV response from {url}"))?;
    let set: GlobalSetResponse = serde_json::from_slice(&body)
        .wrap_err_with(|| format!("Failed to parse 7TV response from {url}"))?;
    Ok(set
        .emotes
        .into_iter()
        .map(|emote| GlobalCatalogEmote {
            id: emote.id,
            name: emote.name,
        })
        .collect())
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
        .send_traced()
        .await
        .wrap_err_with(|| format!("Failed to send 7TV request to {url}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = read_capped(response, MAX_BODY_BYTES)
            .await
            .map(String::from_utf8_lossy_owned)
            .unwrap_or_default();
        bail!("7TV request failed with status {status}: {body}");
    }

    let body = read_capped(response, MAX_BODY_BYTES)
        .await
        .wrap_err_with(|| format!("Failed to read 7TV response from {url}"))?;
    let user: SevenTvUserResponse = serde_json::from_slice(&body)
        .wrap_err_with(|| format!("Failed to parse 7TV response from {url}"))?;
    ChannelSetSnapshot::from_response(user)
}
