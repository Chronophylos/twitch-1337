//! 7TV channel-set REST bootstrap/reconcile + EventAPI websocket ingest loop.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use eyre::{Result, WrapErr as _, eyre};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, warn};

use super::eventapi::{
    EventApiConnection, EventApiSink, ParsedEvent, connect_and_subscribe, live_change_to_batch,
    parse_server_message, subscribe_emote_set_update_message, subscribe_user_update_message,
    unsubscribe_emote_set_update_message,
};
use super::handle::EmoteTrackerHandle;
use super::reconcile::{bootstrap_batch, reconcile_batch};
use super::rest::fetch_channel_set;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Runtime configuration for the emote-set ingest task.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    pub twitch_channel_id: String,
    pub rest_base_url: String,
    pub eventapi_url: String,
}

impl IngestConfig {
    pub fn new(twitch_channel_id: impl Into<String>) -> Self {
        Self {
            twitch_channel_id: twitch_channel_id.into(),
            rest_base_url: "https://7tv.io/v3".into(),
            eventapi_url: super::eventapi::EVENTAPI_URL.into(),
        }
    }
}

/// Result of a REST fetch + catalog bootstrap/reconcile pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupReconcileResult {
    pub user_id: String,
    pub set_id: String,
    pub emote_count: usize,
}

/// REST fetch plus bootstrap (empty catalog) or reconcile (warm catalog).
///
/// `gap_started_at`: `None` on cold start; `Some(ms)` on reconnect or set-switch
/// continuity breaks so reconcile writes an explicit gap row.
pub async fn startup_reconcile_once(
    handle: &EmoteTrackerHandle,
    http: &reqwest::Client,
    cfg: &IngestConfig,
    gap_started_at: Option<i64>,
) -> Result<StartupReconcileResult> {
    let snapshot = fetch_channel_set(http, &cfg.rest_base_url, &cfg.twitch_channel_id).await?;
    let at = Utc::now().timestamp_millis();

    let batch = if handle.catalog_is_empty().await? {
        bootstrap_batch(at, &snapshot.emotes)
    } else {
        let current = handle.list_active_emotes().await?;
        let gap = gap_started_at.map(|from| (from, at));
        reconcile_batch(at, &current, &snapshot.emotes, gap, None)
    };

    handle.apply_set_changes(batch).await?;

    Ok(StartupReconcileResult {
        user_id: snapshot.user_id,
        emote_count: snapshot.emotes.len(),
        set_id: snapshot.set_id,
    })
}

/// Long-running ingest: REST on startup/reconnect, EventAPI live updates.
///
/// Never returns `Err` to the caller — logs and retries with exponential backoff.
pub async fn run_set_ingest(
    handle: Arc<EmoteTrackerHandle>,
    cfg: IngestConfig,
    http: reqwest::Client,
) {
    let mut backoff = INITIAL_BACKOFF;
    let mut gap_started_at: Option<i64> = None;

    loop {
        match startup_reconcile_once(&handle, &http, &cfg, gap_started_at).await {
            Ok(result) => {
                debug!(
                    user_id = %result.user_id,
                    set_id = %result.set_id,
                    emote_count = result.emote_count,
                    "emote set ingest REST reconcile complete"
                );
                gap_started_at = None;
                backoff = INITIAL_BACKOFF;

                match run_eventapi_session(
                    &handle,
                    &http,
                    &cfg,
                    &result.user_id,
                    &result.set_id,
                    &mut gap_started_at,
                )
                .await
                {
                    Ok(remembered_set_id) => {
                        // Session ended cleanly (should not happen); reconnect.
                        let _ = remembered_set_id;
                    }
                    Err(error) => {
                        error!(?error, "emote set ingest EventAPI session ended");
                    }
                }
            }
            Err(error) => {
                error!(?error, "emote set ingest REST reconcile failed");
            }
        }

        if gap_started_at.is_none() {
            gap_started_at = Some(Utc::now().timestamp_millis());
        }
        warn!(
            backoff_secs = backoff.as_secs(),
            "emote set ingest backing off before reconnect"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn run_eventapi_session(
    handle: &EmoteTrackerHandle,
    http: &reqwest::Client,
    cfg: &IngestConfig,
    seventv_user_id: &str,
    initial_set_id: &str,
    gap_started_at: &mut Option<i64>,
) -> Result<String> {
    let mut remembered_set_id = initial_set_id.to_string();
    let mut conn = connect_eventapi(&cfg.eventapi_url, &remembered_set_id, seventv_user_id).await?;
    let mut heartbeat_interval_ms: Option<u64> = None;
    let mut heartbeat_deadline: Option<tokio::time::Instant> = None;

    while let Some(frame) = conn.stream.next().await {
        let msg = match frame {
            Ok(m) => m,
            Err(error) => {
                *gap_started_at = Some(gap_started_at.unwrap_or_else(now_ms));
                return Err(error).wrap_err("EventAPI websocket read error");
            }
        };

        match msg {
            Message::Text(text) => {
                let parsed = match parse_server_message(&text) {
                    Ok(Some(event)) => event,
                    Ok(None) => continue,
                    Err(error) => {
                        warn!(?error, "failed to parse EventAPI frame; skipping");
                        continue;
                    }
                };

                match parsed {
                    ParsedEvent::Hello {
                        heartbeat_interval_ms: hello_interval,
                    } => {
                        heartbeat_interval_ms = hello_interval;
                        if let Some(interval_ms) = hello_interval {
                            let grace = Duration::from_millis(interval_ms.saturating_mul(3));
                            heartbeat_deadline = Some(tokio::time::Instant::now() + grace);
                        }
                    }
                    ParsedEvent::Heartbeat => {
                        if let Some(interval_ms) = heartbeat_interval_ms {
                            let grace = Duration::from_millis(interval_ms.saturating_mul(3));
                            heartbeat_deadline = Some(tokio::time::Instant::now() + grace);
                        }
                    }
                    ParsedEvent::Dispatch(dispatch) => {
                        if dispatch.set_id != remembered_set_id {
                            // Defense in depth: object_id sub should only deliver the
                            // active set. Confirm via REST before treating as a switch.
                            handle_possible_set_switch(
                                handle,
                                http,
                                cfg,
                                gap_started_at,
                                &mut remembered_set_id,
                                Some(&mut conn.sink),
                                &dispatch.set_id,
                            )
                            .await?;
                        } else {
                            let at = now_ms();
                            let batch = live_change_to_batch(at, &dispatch);
                            if !batch.mutations.is_empty() {
                                handle.apply_set_changes(batch).await?;
                            }
                        }
                    }
                    ParsedEvent::UserActiveSetChanged(change) => {
                        if change.new_set_id != remembered_set_id {
                            handle_possible_set_switch(
                                handle,
                                http,
                                cfg,
                                gap_started_at,
                                &mut remembered_set_id,
                                Some(&mut conn.sink),
                                &change.new_set_id,
                            )
                            .await?;
                        }
                    }
                    ParsedEvent::Error { code, message } => {
                        warn!(?code, %message, "EventAPI error frame");
                    }
                    ParsedEvent::EndOfStream { code, message } => {
                        *gap_started_at = Some(gap_started_at.unwrap_or_else(now_ms));
                        return Err(eyre!(
                            "EventAPI end of stream code={code} message={message}"
                        ));
                    }
                    ParsedEvent::Ack | ParsedEvent::Other { .. } => {}
                }

                if let Some(deadline) = heartbeat_deadline
                    && tokio::time::Instant::now() > deadline
                {
                    warn!("EventAPI heartbeat watchdog overdue; continuing ingest");
                    heartbeat_deadline = None;
                }
            }
            Message::Close(frame) => {
                *gap_started_at = Some(gap_started_at.unwrap_or_else(now_ms));
                return Err(match frame {
                    Some(close) => eyre!(
                        "EventAPI websocket closed code={} reason={}",
                        u16::from(close.code),
                        close.reason
                    ),
                    None => eyre!("EventAPI websocket closed"),
                });
            }
            Message::Ping(payload) => {
                let _ = conn.sink.send(Message::Pong(payload)).await;
            }
            _ => {}
        }
    }

    *gap_started_at = Some(gap_started_at.unwrap_or_else(now_ms));
    Err(eyre!("EventAPI websocket stream ended"))
}

/// How to treat a set-id mismatch against the REST active set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SetIdMismatch {
    /// Hint targets a set that is not the channel's current active set.
    Foreign,
    /// Channel active emote set changed; REST reports `active_set_id`.
    ActiveSetChanged { active_set_id: String },
}

fn classify_set_id_mismatch(remembered_set_id: &str, rest_active_set_id: &str) -> SetIdMismatch {
    if rest_active_set_id == remembered_set_id {
        SetIdMismatch::Foreign
    } else {
        SetIdMismatch::ActiveSetChanged {
            active_set_id: rest_active_set_id.to_string(),
        }
    }
}

/// Confirm a hinted set-id change via REST; on real switch, reconcile + resubscribe.
async fn handle_possible_set_switch(
    handle: &EmoteTrackerHandle,
    http: &reqwest::Client,
    cfg: &IngestConfig,
    gap_started_at: &mut Option<i64>,
    remembered_set_id: &mut String,
    sink: Option<&mut EventApiSink>,
    hinted_set_id: &str,
) -> Result<()> {
    let snapshot = fetch_channel_set(http, &cfg.rest_base_url, &cfg.twitch_channel_id).await?;
    match classify_set_id_mismatch(remembered_set_id, &snapshot.set_id) {
        SetIdMismatch::Foreign => {
            debug!(
                remembered_set_id = %remembered_set_id,
                hinted_set_id = %hinted_set_id,
                "ignoring EventAPI hint for non-active set"
            );
            Ok(())
        }
        SetIdMismatch::ActiveSetChanged { active_set_id } => {
            warn!(
                old_set_id = %remembered_set_id,
                new_set_id = %active_set_id,
                hinted_set_id = %hinted_set_id,
                "7TV emote set switch detected"
            );
            let gap_from = gap_started_at.unwrap_or_else(|| {
                let at = now_ms();
                *gap_started_at = Some(at);
                at
            });

            let at = now_ms();
            let current = handle.list_active_emotes().await?;
            let batch = reconcile_batch(at, &current, &snapshot.emotes, Some((gap_from, at)), None);
            handle.apply_set_changes(batch).await?;

            if let Some(sink) = sink {
                resubscribe_emote_set(sink, remembered_set_id, &active_set_id).await?;
            }

            *remembered_set_id = active_set_id;
            *gap_started_at = None;
            Ok(())
        }
    }
}

async fn resubscribe_emote_set(
    sink: &mut EventApiSink,
    old_set_id: &str,
    new_set_id: &str,
) -> Result<()> {
    if old_set_id == new_set_id {
        return Ok(());
    }
    let unsub = unsubscribe_emote_set_update_message(old_set_id);
    if let Err(error) = sink.send(Message::Text(unsub.into())).await {
        warn!(?error, old_set_id, "failed to unsubscribe old emote set");
    }
    let sub = subscribe_emote_set_update_message(new_set_id);
    sink.send(Message::Text(sub.into()))
        .await
        .wrap_err("failed to subscribe new emote set after switch")?;
    Ok(())
}

async fn connect_eventapi(
    eventapi_url: &str,
    emote_set_id: &str,
    seventv_user_id: &str,
) -> Result<EventApiConnection> {
    if eventapi_url == super::eventapi::EVENTAPI_URL {
        return connect_and_subscribe(emote_set_id, seventv_user_id).await;
    }

    let (ws, _) = connect_async(eventapi_url)
        .await
        .wrap_err_with(|| format!("failed to connect to EventAPI at {eventapi_url}"))?;
    let (mut sink, stream) = ws.split();
    for sub in [
        subscribe_emote_set_update_message(emote_set_id),
        subscribe_user_update_message(seventv_user_id),
    ] {
        sink.send(Message::Text(sub.into()))
            .await
            .wrap_err("failed to send EventAPI subscribe")?;
    }
    Ok(EventApiConnection { sink, stream })
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::{DB_FILENAME, types::SetEventKind};
    use rusqlite::Connection;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn count_events(path: &std::path::Path, kind: SetEventKind) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM set_events WHERE kind = ?1",
            [kind.as_str()],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn user_set_body(user_id: &str, set_id: &str, emotes_json: &str) -> String {
        format!(
            r#"{{
            "id": "784681545",
            "platform": "TWITCH",
            "user": {{ "id": "{user_id}", "username": "streamer" }},
            "emote_set": {{
                "id": "{set_id}",
                "emotes": {emotes_json}
            }}
        }}"#
        )
    }

    #[tokio::test]
    async fn startup_reconcile_cold_start_bootstraps_without_gap() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        let channel_id = "12345";
        let body = user_set_body(
            "01USER",
            "set123",
            r#"[{"id": "e1", "name": "Kappa"}, {"id": "e2", "name": "Pog"}]"#,
        );
        Mock::given(method("GET"))
            .and(path(format!("/v3/users/twitch/{channel_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(DB_FILENAME);
        let handle = EmoteTrackerHandle::open(&db_path).unwrap();

        let cfg = IngestConfig {
            twitch_channel_id: channel_id.into(),
            rest_base_url: format!("{}/v3", mock.uri()),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
        };

        let result = startup_reconcile_once(&handle, &reqwest::Client::new(), &cfg, None)
            .await
            .unwrap();

        assert_eq!(result.user_id, "01USER");
        assert_eq!(result.set_id, "set123");
        assert_eq!(result.emote_count, 2);
        assert_eq!(count_events(&db_path, SetEventKind::Bootstrap), 2);
        assert_eq!(count_events(&db_path, SetEventKind::Gap), 0);

        let active = handle.list_active_emotes().await.unwrap();
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn foreign_set_update_when_rest_active_unchanged() {
        // Prod symptom: channel sub delivers NNYS set updates while the
        // channel active set stays 01FZKKA0A8000FP66RDQP75B9A.
        let remembered = "01FZKKA0A8000FP66RDQP75B9A";
        let rest_active = "01FZKKA0A8000FP66RDQP75B9A";
        assert_eq!(
            classify_set_id_mismatch(remembered, rest_active),
            SetIdMismatch::Foreign
        );
    }

    #[test]
    fn active_set_changed_when_rest_reports_new_id() {
        let remembered = "01FZKKA0A8000FP66RDQP75B9A";
        let rest_active = "01NEWACTIVESET000000000000";
        assert_eq!(
            classify_set_id_mismatch(remembered, rest_active),
            SetIdMismatch::ActiveSetChanged {
                active_set_id: rest_active.into(),
            }
        );
    }

    #[tokio::test]
    async fn foreign_set_dispatch_does_not_write_gap() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        let channel_id = "784681545";
        let active_set = "01FZKKA0A8000FP66RDQP75B9A";
        let body = user_set_body(active_set, active_set, r#"[{"id": "e1", "name": "Kappa"}]"#);
        Mock::given(method("GET"))
            .and(path(format!("/v3/users/twitch/{channel_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(DB_FILENAME);
        let handle = EmoteTrackerHandle::open(&db_path).unwrap();
        let cfg = IngestConfig {
            twitch_channel_id: channel_id.into(),
            rest_base_url: format!("{}/v3", mock.uri()),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
        };

        startup_reconcile_once(&handle, &reqwest::Client::new(), &cfg, None)
            .await
            .unwrap();
        assert_eq!(count_events(&db_path, SetEventKind::Gap), 0);

        let mut remembered = active_set.to_string();
        let mut gap_started_at = None;
        handle_possible_set_switch(
            &handle,
            &reqwest::Client::new(),
            &cfg,
            &mut gap_started_at,
            &mut remembered,
            None,
            "01JEY2SMVTRMC4ZHWCYKBA6M5R", // NNYS 2024 — not the active set
        )
        .await
        .unwrap();

        assert_eq!(remembered, active_set);
        assert_eq!(count_events(&db_path, SetEventKind::Gap), 0);
        assert!(gap_started_at.is_none());
    }

    #[tokio::test]
    async fn real_active_set_switch_still_reconciles_with_gap() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        let channel_id = "784681545";
        let user_id = "01FZKKA0A8000FP66RDQP75B9A";
        let old_set = "01FZKKA0A8000FP66RDQP75B9A";
        let new_set = "01NEWACTIVESET000000000000";

        let initial = user_set_body(user_id, old_set, r#"[{"id": "e1", "name": "Kappa"}]"#);
        let switched = user_set_body(
            user_id,
            new_set,
            r#"[{"id": "e1", "name": "Kappa"}, {"id": "e2", "name": "Pog"}]"#,
        );

        Mock::given(method("GET"))
            .and(path(format!("/v3/users/twitch/{channel_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(initial))
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/v3/users/twitch/{channel_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(switched))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(DB_FILENAME);
        let handle = EmoteTrackerHandle::open(&db_path).unwrap();
        let cfg = IngestConfig {
            twitch_channel_id: channel_id.into(),
            rest_base_url: format!("{}/v3", mock.uri()),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
        };

        startup_reconcile_once(&handle, &reqwest::Client::new(), &cfg, None)
            .await
            .unwrap();

        let mut remembered = old_set.to_string();
        let mut gap_started_at = None;
        handle_possible_set_switch(
            &handle,
            &reqwest::Client::new(),
            &cfg,
            &mut gap_started_at,
            &mut remembered,
            None,
            new_set,
        )
        .await
        .unwrap();

        assert_eq!(remembered, new_set);
        assert_eq!(count_events(&db_path, SetEventKind::Gap), 1);
        assert_eq!(count_events(&db_path, SetEventKind::Add), 1);
        assert!(gap_started_at.is_none());
    }
}
