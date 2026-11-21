use std::collections::VecDeque;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::util::http::{SendTraced as _, read_capped};

fn default_base_url() -> String {
    "https://logs.zonian.dev".to_string()
}

fn default_threshold() -> f64 {
    0.5
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HistoryPrefillConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
}

impl From<&crate::settings::ai::AiPrefill> for HistoryPrefillConfig {
    fn from(p: &crate::settings::ai::AiPrefill) -> Self {
        Self {
            base_url: p.base_url.clone(),
            threshold: p.threshold,
        }
    }
}

#[derive(Deserialize)]
struct LogResponse {
    messages: Vec<LogMessage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogMessage {
    display_name: String,
    text: String,
}

/// Fetch messages for a specific date from the rustlog API.
///
/// Returns messages in chronological order (oldest first).
/// On any error, logs a warning and returns an empty Vec.
async fn fetch_messages_for_date(
    http: &reqwest::Client,
    base_url: &str,
    channel: &str,
    date: jiff::civil::Date,
    limit: usize,
) -> Vec<(String, String, Timestamp)> {
    let url = format!(
        "{}/channel/{}/{}/{}/{}?jsonBasic=1&limit={}&reverse=1",
        base_url.trim_end_matches('/'),
        channel,
        date.year(),
        date.month(),
        date.day(),
        limit,
    );

    debug!(url = %url, "Fetching chat history");

    let response = match http.get(&url).send_traced().await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = ?e, url = %url, "Failed to fetch chat history");
            return Vec::new();
        }
    };

    if !response.status().is_success() {
        warn!(
            status = %response.status(),
            url = %url,
            "Chat history API returned non-success status"
        );
        return Vec::new();
    }

    let body = match read_capped(response, PREFILL_MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            warn!(error = ?e, url = %url, "Failed to read chat history response");
            return Vec::new();
        }
    };
    let log_response: LogResponse = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!(error = ?e, url = %url, "Failed to parse chat history response");
            return Vec::new();
        }
    };

    // API returns newest-first with reverse=1, so reverse to get chronological order.
    // Prefilled messages lack exact timestamps; use the fetch time as a proxy.
    let fetched_at = Timestamp::now();
    log_response
        .messages
        .into_iter()
        .rev()
        .map(|msg| (msg.display_name, msg.text, fetched_at))
        .collect()
}

const PREFILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// A day page holds at most `history_length` short chat lines; anything near
/// this is not a rustlog answer.
const PREFILL_MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

/// Prefill the chat history buffer by fetching recent messages from a rustlog-compatible API.
///
/// Fetches today's messages. If the count is below `config.threshold * history_length`,
/// also fetches yesterday's messages. Returns at most `history_length` messages in
/// chronological order.
///
/// On any failure, logs a warning and returns what it has (or an empty buffer).
pub async fn prefill_chat_history(
    channel: &str,
    history_length: usize,
    config: &HistoryPrefillConfig,
) -> VecDeque<(String, String, Timestamp)> {
    // The log service is third-party: never follow its redirects (rustlog
    // answers the day URL directly), so it cannot point the bot at an
    // internal host.
    let http = match reqwest::Client::builder()
        .timeout(PREFILL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            warn!(error = ?e, "Failed to create HTTP client for history prefill");
            return VecDeque::with_capacity(history_length);
        }
    };

    let now = crate::util::to_berlin(Timestamp::now());
    let today = now.date();
    #[expect(
        clippy::expect_used,
        reason = "yesterday underflows only at jiff's minimum date"
    )]
    let yesterday = today.yesterday().expect("valid prev day");

    // Fetch today's messages
    let today_messages =
        fetch_messages_for_date(&http, &config.base_url, channel, today, history_length).await;

    let threshold_count = (config.threshold * history_length as f64).ceil() as usize;
    let today_count = today_messages.len();

    // If today has fewer messages than the threshold, also fetch yesterday
    let mut all_messages = if today_count < threshold_count {
        debug!(
            today_count,
            threshold_count, "Today's messages below threshold, fetching yesterday"
        );
        let yesterday_messages =
            fetch_messages_for_date(&http, &config.base_url, channel, yesterday, history_length)
                .await;

        let mut combined = yesterday_messages;
        combined.extend(today_messages);
        combined
    } else {
        today_messages
    };

    // Take only the last history_length messages
    if all_messages.len() > history_length {
        all_messages.drain(..all_messages.len() - history_length);
    }

    let count = all_messages.len();
    info!(count, "Prefilled chat history buffer");

    VecDeque::from(all_messages)
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn config(base_url: String) -> HistoryPrefillConfig {
        HistoryPrefillConfig {
            base_url,
            threshold: 0.0,
        }
    }

    #[tokio::test]
    async fn redirect_from_log_service_is_not_followed() {
        crate::install_crypto_provider();
        let internal = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "messages": [{ "displayName": "internal_host", "text": "leaked" }]
            })))
            .expect(0)
            .mount(&internal)
            .await;
        let logs = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("^/channel/"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/internal/status", internal.uri())),
            )
            .mount(&logs)
            .await;

        let rows = prefill_chat_history("somechannel", 10, &config(logs.uri())).await;
        assert!(rows.is_empty(), "{rows:?}");
        // `expect(0)` on `internal` is verified when it drops.
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        crate::install_crypto_provider();
        let logs = MockServer::start().await;
        let huge = format!(
            r#"{{"messages":[{{"displayName":"a","text":"{}"}}]}}"#,
            "x".repeat(PREFILL_MAX_BODY_BYTES as usize)
        );
        Mock::given(method("GET"))
            .and(path_regex("^/channel/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(huge))
            .mount(&logs)
            .await;

        let rows = prefill_chat_history("somechannel", 10, &config(logs.uri())).await;
        assert!(rows.is_empty());
    }
}
