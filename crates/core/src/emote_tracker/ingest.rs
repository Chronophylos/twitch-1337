//! 7TV channel-set REST bootstrap/reconcile + EventAPI websocket ingest loop.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use eyre::{Result, WrapErr as _, eyre};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use super::eventapi::{
    EventApiConnection, EventApiSink, ParsedEvent, connect_and_subscribe, is_expected_reconnect,
    live_change_to_batch, parse_server_message, subscribe_emote_set_update_message,
    subscribe_user_update_message, unsubscribe_emote_set_update_message,
};
use super::handle::EmoteTrackerHandle;
use super::potat_attribution::{
    HISTORY_RETRY_DELAY, POTAT_HISTORY_BASE_URL, SMALL_SLACK_SECS, fetch_potat_history,
    mediation_gate_open, resolve_requester_login,
};
use super::reconcile::{bootstrap_batch, reconcile_batch};
use super::rest::fetch_channel_set;
use super::types::{NewSetEvent, SetChangeBatch, SetEventKind};
use crate::settings::{PotatAttributionSettings, SettingsHandle};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// EventAPI close `4007 Maintenance` — docs ask for ≥5 minutes including jitter.
const MAINTENANCE_BACKOFF: Duration = Duration::from_secs(300);

/// How an EventAPI websocket session ended.
#[derive(Debug)]
enum SessionEnd {
    /// Server asked for a reconnect (soft); REST reconcile without a gap stamp.
    ExpectedReconnect {
        code: u16,
        message: String,
        remembered_set_id: String,
    },
}

/// Runtime configuration for the emote-set ingest task.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    pub twitch_channel_id: String,
    /// Twitch login for Potat history URLs (`config.twitch.channel`).
    pub twitch_channel_login: String,
    pub rest_base_url: String,
    pub eventapi_url: String,
    /// Override for tests (`wiremock`); production uses [`POTAT_HISTORY_BASE_URL`].
    pub potat_history_base_url: String,
}

impl IngestConfig {
    pub fn new(
        twitch_channel_id: impl Into<String>,
        twitch_channel_login: impl Into<String>,
    ) -> Self {
        Self {
            twitch_channel_id: twitch_channel_id.into(),
            twitch_channel_login: twitch_channel_login.into(),
            rest_base_url: "https://7tv.io/v3".into(),
            eventapi_url: super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: POTAT_HISTORY_BASE_URL.into(),
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
    settings: SettingsHandle,
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
                    &settings,
                    &result.user_id,
                    &result.set_id,
                    &mut gap_started_at,
                )
                .await
                {
                    Ok(SessionEnd::ExpectedReconnect {
                        code,
                        message,
                        remembered_set_id,
                    }) => {
                        // Soft reconnect: keep listening without stamping a gap.
                        // Next loop REST-reconciles (gap=None) to catch anything
                        // missed in the brief window.
                        let _ = remembered_set_id;
                        info!(
                            code,
                            %message,
                            "EventAPI requested reconnect; resuming without gap"
                        );
                        if code == 4007 {
                            backoff = MAINTENANCE_BACKOFF;
                        }
                    }
                    Err(error) => {
                        error!(?error, "emote set ingest EventAPI session ended");
                        if gap_started_at.is_none() {
                            gap_started_at = Some(Utc::now().timestamp_millis());
                        }
                    }
                }
            }
            Err(error) => {
                error!(?error, "emote set ingest REST reconcile failed");
                if gap_started_at.is_none() {
                    gap_started_at = Some(Utc::now().timestamp_millis());
                }
            }
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
    settings: &SettingsHandle,
    seventv_user_id: &str,
    initial_set_id: &str,
    gap_started_at: &mut Option<i64>,
) -> Result<SessionEnd> {
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
                            let mut batch = live_change_to_batch(at, &dispatch);
                            maybe_attribute_batch(http, settings, cfg, &mut batch).await;
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
                        if is_expected_reconnect(code) {
                            return Ok(SessionEnd::ExpectedReconnect {
                                code,
                                message,
                                remembered_set_id,
                            });
                        }
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
                if let Some(close) = &frame {
                    let code = u16::from(close.code);
                    if is_expected_reconnect(code) {
                        return Ok(SessionEnd::ExpectedReconnect {
                            code,
                            message: close.reason.to_string(),
                            remembered_set_id,
                        });
                    }
                }
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

fn event_is_attribute_eligible(event: &NewSetEvent) -> bool {
    matches!(
        event.kind,
        SetEventKind::Add | SetEventKind::Remove | SetEventKind::Rename
    ) && event.emote_id.is_some()
}

fn attribute_one(
    event: &mut NewSetEvent,
    history: &[super::potat_attribution::PotatHistoryItem],
    potat: &PotatAttributionSettings,
    soft_cap: u64,
) {
    let Some(emote_id) = event.emote_id.as_deref() else {
        return;
    };
    if !event_is_attribute_eligible(event) {
        return;
    }
    if !mediation_gate_open(
        event.actor_7tv_id.as_deref(),
        &potat.potat_7tv_user_id,
        history,
        emote_id,
        soft_cap,
    ) {
        return;
    }
    let Some(login) = resolve_requester_login(history, emote_id, event.kind, soft_cap) else {
        return;
    };
    event.requester_login = Some(login);
    event.via = Some("potatbotat".into());
    event.attribution_method = Some("potat_history".into());
}

fn any_step_b_hit(
    batch: &SetChangeBatch,
    history: &[super::potat_attribution::PotatHistoryItem],
    potat: &PotatAttributionSettings,
    soft_cap: u64,
) -> bool {
    batch.events.iter().any(|event| {
        let Some(emote_id) = event.emote_id.as_deref() else {
            return false;
        };
        if !event_is_attribute_eligible(event) {
            return false;
        }
        if !mediation_gate_open(
            event.actor_7tv_id.as_deref(),
            &potat.potat_7tv_user_id,
            history,
            emote_id,
            soft_cap,
        ) {
            return false;
        }
        resolve_requester_login(history, emote_id, event.kind, soft_cap).is_some()
    })
}

async fn fetch_with_retry(
    http: &reqwest::Client,
    cfg: &IngestConfig,
    batch: &SetChangeBatch,
    potat: &PotatAttributionSettings,
    soft_cap: u64,
) -> eyre::Result<Vec<super::potat_attribution::PotatHistoryItem>> {
    let first =
        fetch_potat_history(http, &cfg.potat_history_base_url, &cfg.twitch_channel_login).await?;
    if any_step_b_hit(batch, &first, potat, soft_cap)
        || !batch.events.iter().any(event_is_attribute_eligible)
    {
        return Ok(first);
    }
    tokio::time::sleep(HISTORY_RETRY_DELAY).await;
    fetch_potat_history(http, &cfg.potat_history_base_url, &cfg.twitch_channel_login).await
}

/// Settings-gated Potat history attribution for one live EventAPI dispatch.
pub async fn maybe_attribute_batch(
    http: &reqwest::Client,
    settings: &SettingsHandle,
    cfg: &IngestConfig,
    batch: &mut SetChangeBatch,
) {
    let snap = settings.load();
    let potat = &snap.emote_tracker.potat_attribution;
    if !potat.enabled {
        return;
    }
    if !batch.events.iter().any(event_is_attribute_eligible) {
        return;
    }
    let soft_cap = potat.window_secs.saturating_add(SMALL_SLACK_SECS);
    let history = match fetch_with_retry(http, cfg, batch, potat, soft_cap).await {
        Ok(h) => h,
        Err(error) => {
            warn!(?error, "potat history fetch failed; skipping attribution");
            return;
        }
    };
    for event in &mut batch.events {
        attribute_one(event, &history, potat, soft_cap);
    }
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
    use crate::settings::{PotatAttributionSettings, Settings};
    use arc_swap::ArcSwap;
    use rusqlite::Connection;
    use std::sync::Arc;
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
            twitch_channel_login: "testchannel".into(),
            rest_base_url: format!("{}/v3", mock.uri()),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: POTAT_HISTORY_BASE_URL.into(),
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
            twitch_channel_login: "testchannel".into(),
            rest_base_url: format!("{}/v3", mock.uri()),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: POTAT_HISTORY_BASE_URL.into(),
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
            twitch_channel_login: "testchannel".into(),
            rest_base_url: format!("{}/v3", mock.uri()),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: POTAT_HISTORY_BASE_URL.into(),
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

    fn potat_settings_handle(enabled: bool) -> SettingsHandle {
        let mut s = Settings::compiled_defaults();
        s.emote_tracker.potat_attribution = PotatAttributionSettings {
            enabled,
            window_secs: 15,
            potat_7tv_user_id: "01GNRG2MDG0000DM7N933JH49Y".into(),
        };
        Arc::new(ArcSwap::from_pointee(s))
    }

    fn add_event(emote_id: &str, actor_7tv_id: Option<&str>) -> NewSetEvent {
        NewSetEvent {
            at: 1,
            kind: SetEventKind::Add,
            emote_id: Some(emote_id.into()),
            old_name: None,
            new_name: Some("Kappa".into()),
            actor_7tv_id: actor_7tv_id.map(str::to_owned),
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
            requester_login: None,
            requester_twitch_id: None,
            via: None,
            attribution_method: None,
        }
    }

    #[tokio::test]
    async fn live_batch_attributes_from_mocked_potat_history() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        let login = "euterheissgetraenk";
        let body = include_str!("fixtures/potat_history_standard.json");
        Mock::given(method("GET"))
            .and(path(format!("/emotes/history/{login}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = IngestConfig {
            twitch_channel_id: "123".into(),
            twitch_channel_login: login.into(),
            rest_base_url: "https://7tv.io/v3".into(),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: format!("{}/emotes/history", mock.uri()),
        };
        let settings = potat_settings_handle(true);
        let mut batch = SetChangeBatch {
            updated_at: 1,
            mutations: vec![],
            events: vec![add_event("e1", Some("01GNRG2MDG0000DM7N933JH49Y"))],
        };

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert_eq!(batch.events[0].requester_login.as_deref(), Some("ringomar"));
        assert_eq!(batch.events[0].via.as_deref(), Some("potatbotat"));
        assert_eq!(
            batch.events[0].attribution_method.as_deref(),
            Some("potat_history")
        );
    }

    #[tokio::test]
    async fn attribution_skipped_when_disabled() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        let login = "euterheissgetraenk";
        Mock::given(method("GET"))
            .and(path(format!("/emotes/history/{login}")))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(0)
            .mount(&mock)
            .await;

        let cfg = IngestConfig {
            twitch_channel_id: "123".into(),
            twitch_channel_login: login.into(),
            rest_base_url: "https://7tv.io/v3".into(),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: format!("{}/emotes/history", mock.uri()),
        };
        let settings = potat_settings_handle(false);
        let mut batch = SetChangeBatch {
            updated_at: 1,
            mutations: vec![],
            events: vec![add_event("e1", Some("01GNRG2MDG0000DM7N933JH49Y"))],
        };

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert!(batch.events[0].requester_login.is_none());
        assert!(batch.events[0].via.is_none());
    }

    #[tokio::test]
    async fn multi_emote_batch_fetches_history_once() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        let login = "euterheissgetraenk";
        let body = r#"{
          "data": [{
            "history": [
              {
                "user_login": "alice",
                "actor": "potatbotat",
                "action": "ADD",
                "type": "STANDARD",
                "emote_id": "e1",
                "provider": "STV",
                "ago": "5s ago"
              },
              {
                "user_login": "bob",
                "actor": "potatbotat",
                "action": "ADD",
                "type": "STANDARD",
                "emote_id": "e2",
                "provider": "STV",
                "ago": "5s ago"
              }
            ]
          }]
        }"#;
        Mock::given(method("GET"))
            .and(path(format!("/emotes/history/{login}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = IngestConfig {
            twitch_channel_id: "123".into(),
            twitch_channel_login: login.into(),
            rest_base_url: "https://7tv.io/v3".into(),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: format!("{}/emotes/history", mock.uri()),
        };
        let settings = potat_settings_handle(true);
        let mut batch = SetChangeBatch {
            updated_at: 1,
            mutations: vec![],
            events: vec![
                add_event("e1", Some("01GNRG2MDG0000DM7N933JH49Y")),
                add_event("e2", Some("01GNRG2MDG0000DM7N933JH49Y")),
            ],
        };

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert_eq!(batch.events[0].requester_login.as_deref(), Some("alice"));
        assert_eq!(batch.events[1].requester_login.as_deref(), Some("bob"));
    }

    #[tokio::test]
    async fn soft_fail_leaves_attribution_null_on_http_error() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        let login = "euterheissgetraenk";
        Mock::given(method("GET"))
            .and(path(format!("/emotes/history/{login}")))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let cfg = IngestConfig {
            twitch_channel_id: "123".into(),
            twitch_channel_login: login.into(),
            rest_base_url: "https://7tv.io/v3".into(),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: format!("{}/emotes/history", mock.uri()),
        };
        let settings = potat_settings_handle(true);
        let mut batch = SetChangeBatch {
            updated_at: 1,
            mutations: vec![],
            events: vec![add_event("e1", Some("01GNRG2MDG0000DM7N933JH49Y"))],
        };

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert!(batch.events[0].requester_login.is_none());
    }
}
