//! 7TV channel-set REST bootstrap/reconcile + EventAPI websocket ingest loop.

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, WrapErr as _, eyre};
use futures_util::{SinkExt, StreamExt};
use jiff::Timestamp;
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
use crate::lifecycle::Lifecycle;
use crate::settings::{PotatAttributionSettings, SettingsHandle};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// EventAPI close `4007 Maintenance` — docs ask for ≥5 minutes including jitter.
const MAINTENANCE_BACKOFF: Duration = Duration::from_secs(300);

/// How an EventAPI websocket session ended.
#[derive(Debug)]
enum SessionEnd {
    /// Lifecycle cancellation won at an operation boundary.
    Cancelled,
    /// Server asked for a reconnect (soft); REST reconcile without a gap stamp.
    ExpectedReconnect { code: u16, message: String },
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "compatibility seam for callers outside the production loop"
    )
)]
pub async fn startup_reconcile_once(
    handle: &EmoteTrackerHandle,
    http: &reqwest::Client,
    cfg: &IngestConfig,
    gap_started_at: Option<i64>,
) -> Result<StartupReconcileResult> {
    let lifecycle = Lifecycle::new();
    startup_reconcile_once_until(handle, http, cfg, gap_started_at, &lifecycle)
        .await?
        .ok_or_else(|| eyre!("startup reconcile unexpectedly cancelled"))
}

async fn startup_reconcile_once_until(
    handle: &EmoteTrackerHandle,
    http: &reqwest::Client,
    cfg: &IngestConfig,
    gap_started_at: Option<i64>,
    lifecycle: &Lifecycle,
) -> Result<Option<StartupReconcileResult>> {
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    let snapshot = fetch_channel_set(http, &cfg.rest_base_url, &cfg.twitch_channel_id).await?;
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    let at = Timestamp::now().as_millisecond();

    let empty = handle.catalog_is_empty().await?;
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    let batch = if empty {
        bootstrap_batch(at, &snapshot.emotes)
    } else {
        let current = handle.list_active_emotes().await?;
        if lifecycle.is_cancelled() {
            return Ok(None);
        }
        let gap = gap_started_at.map(|from| (from, at));
        reconcile_batch(at, &current, &snapshot.emotes, gap, None)
    };

    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    handle.apply_set_changes(batch).await?;

    Ok(Some(StartupReconcileResult {
        user_id: snapshot.user_id,
        emote_count: snapshot.emotes.len(),
        set_id: snapshot.set_id,
    }))
}

/// Long-running ingest: REST on startup/reconnect, EventAPI live updates.
///
/// Never returns `Err` to the caller — logs and retries with exponential backoff.
pub async fn run_set_ingest(
    handle: Arc<EmoteTrackerHandle>,
    cfg: IngestConfig,
    http: reqwest::Client,
    settings: SettingsHandle,
    lifecycle: Lifecycle,
) {
    let mut backoff = INITIAL_BACKOFF;
    let mut gap_started_at: Option<i64> = None;

    loop {
        if lifecycle.is_cancelled() {
            info!("emote set ingest lifecycle cancelled, shutting down");
            return;
        }
        // A server-requested reconnect is routine; only unexpected exits warn.
        let mut expected = false;
        match startup_reconcile_once_until(&handle, &http, &cfg, gap_started_at, &lifecycle).await {
            Ok(Some(result)) => {
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
                    &lifecycle,
                )
                .await
                {
                    Ok(SessionEnd::Cancelled) => return,
                    Ok(SessionEnd::ExpectedReconnect { code, message }) => {
                        // Soft reconnect: keep listening without stamping a gap.
                        // Next loop REST-reconciles (gap=None) to catch anything
                        // missed in the brief window.
                        info!(
                            code,
                            %message,
                            "EventAPI requested reconnect; resuming without gap"
                        );
                        if code == 4007 {
                            backoff = MAINTENANCE_BACKOFF;
                        }
                        expected = true;
                    }
                    Err(error) => {
                        error!(?error, "emote set ingest EventAPI session ended");
                        if gap_started_at.is_none() {
                            gap_started_at = Some(Timestamp::now().as_millisecond());
                        }
                    }
                }
            }
            Ok(None) => return,
            Err(error) => {
                error!(?error, "emote set ingest REST reconcile failed");
                if gap_started_at.is_none() {
                    gap_started_at = Some(Timestamp::now().as_millisecond());
                }
            }
        }

        if expected {
            debug!(
                backoff_secs = backoff.as_secs(),
                "emote set ingest soft reconnect"
            );
        } else {
            warn!(
                backoff_secs = backoff.as_secs(),
                "emote set ingest backing off before reconnect"
            );
        }
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one EventAPI session retains its store, clients, config, state, and lifecycle"
)]
async fn run_eventapi_session(
    handle: &EmoteTrackerHandle,
    http: &reqwest::Client,
    cfg: &IngestConfig,
    settings: &SettingsHandle,
    seventv_user_id: &str,
    initial_set_id: &str,
    gap_started_at: &mut Option<i64>,
    lifecycle: &Lifecycle,
) -> Result<SessionEnd> {
    if lifecycle.is_cancelled() {
        return Ok(SessionEnd::Cancelled);
    }
    let mut remembered_set_id = initial_set_id.to_string();
    // Connection plus initial subscriptions form one atomic network operation.
    let mut conn = connect_eventapi(&cfg.eventapi_url, &remembered_set_id, seventv_user_id).await?;
    if lifecycle.is_cancelled() {
        return Ok(SessionEnd::Cancelled);
    }
    let mut heartbeat_interval_ms: Option<u64> = None;
    let mut heartbeat_deadline: Option<tokio::time::Instant> = None;

    loop {
        let frame = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return Ok(SessionEnd::Cancelled),
            frame = conn.stream.next() => frame,
        };
        let Some(frame) = frame else {
            break;
        };
        if lifecycle.is_cancelled() {
            return Ok(SessionEnd::Cancelled);
        }
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
                            if handle_possible_set_switch(
                                handle,
                                http,
                                cfg,
                                gap_started_at,
                                &mut remembered_set_id,
                                Some(&mut conn.sink),
                                &dispatch.set_id,
                                lifecycle,
                            )
                            .await?
                            .is_none()
                            {
                                return Ok(SessionEnd::Cancelled);
                            }
                        } else {
                            let at = now_ms();
                            let mut batch = live_change_to_batch(at, &dispatch);
                            if !maybe_attribute_batch_until(
                                http, settings, cfg, &mut batch, lifecycle,
                            )
                            .await
                            {
                                return Ok(SessionEnd::Cancelled);
                            }
                            if !batch.mutations.is_empty() {
                                if lifecycle.is_cancelled() {
                                    return Ok(SessionEnd::Cancelled);
                                }
                                handle.apply_set_changes(batch).await?;
                            }
                        }
                    }
                    ParsedEvent::UserActiveSetChanged(change) => {
                        if change.new_set_id != remembered_set_id
                            && handle_possible_set_switch(
                                handle,
                                http,
                                cfg,
                                gap_started_at,
                                &mut remembered_set_id,
                                Some(&mut conn.sink),
                                &change.new_set_id,
                                lifecycle,
                            )
                            .await?
                            .is_none()
                        {
                            return Ok(SessionEnd::Cancelled);
                        }
                    }
                    ParsedEvent::Error { code, message } => {
                        warn!(?code, %message, "EventAPI error frame");
                    }
                    ParsedEvent::EndOfStream { code, message } => {
                        if is_expected_reconnect(code) {
                            return Ok(SessionEnd::ExpectedReconnect { code, message });
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
                if lifecycle.is_cancelled() {
                    return Ok(SessionEnd::Cancelled);
                }
                if let Err(error) = conn.sink.send(Message::Pong(payload)).await {
                    warn!(?error, "failed to answer EventAPI ping");
                }
            }
            _ => {}
        }
    }

    if lifecycle.is_cancelled() {
        return Ok(SessionEnd::Cancelled);
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

/// Known non-Potat performers permanently close Step A — history can never reopen them.
fn performer_gate_permanently_closed(actor_7tv_id: Option<&str>, potat_7tv_user_id: &str) -> bool {
    matches!(actor_7tv_id, Some(id) if id != potat_7tv_user_id)
}

/// True when at least one eligible event has null or Potat performer (history may help).
fn batch_needs_history_fetch(batch: &SetChangeBatch, potat_7tv_user_id: &str) -> bool {
    batch.events.iter().any(|event| {
        event_is_attribute_eligible(event)
            && !performer_gate_permanently_closed(event.actor_7tv_id.as_deref(), potat_7tv_user_id)
    })
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
    lifecycle: &Lifecycle,
) -> eyre::Result<Option<Vec<super::potat_attribution::PotatHistoryItem>>> {
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    let first =
        fetch_potat_history(http, &cfg.potat_history_base_url, &cfg.twitch_channel_login).await?;
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    if any_step_b_hit(batch, &first, potat, soft_cap) {
        return Ok(Some(first));
    }
    // Do not sleep+retry when every eligible event has a known non-Potat
    // performer — Step A is permanently closed and a second fetch cannot help.
    if !batch_needs_history_fetch(batch, &potat.potat_7tv_user_id) {
        return Ok(Some(first));
    }
    tokio::select! {
        biased;
        _ = lifecycle.wait_for_cancellation() => return Ok(None),
        _ = tokio::time::sleep(HISTORY_RETRY_DELAY) => {}
    }
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    fetch_potat_history(http, &cfg.potat_history_base_url, &cfg.twitch_channel_login)
        .await
        .map(Some)
}

/// Settings-gated Potat history attribution for one live EventAPI dispatch.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "compatibility seam for callers outside the production loop"
    )
)]
pub async fn maybe_attribute_batch(
    http: &reqwest::Client,
    settings: &SettingsHandle,
    cfg: &IngestConfig,
    batch: &mut SetChangeBatch,
) {
    let lifecycle = Lifecycle::new();
    maybe_attribute_batch_until(http, settings, cfg, batch, &lifecycle).await;
}

async fn maybe_attribute_batch_until(
    http: &reqwest::Client,
    settings: &SettingsHandle,
    cfg: &IngestConfig,
    batch: &mut SetChangeBatch,
    lifecycle: &Lifecycle,
) -> bool {
    if lifecycle.is_cancelled() {
        return false;
    }
    let snap = settings.load();
    let potat = &snap.emote_tracker.potat_attribution;
    if !potat.enabled {
        return true;
    }
    if !batch_needs_history_fetch(batch, &potat.potat_7tv_user_id) {
        return true;
    }
    let soft_cap = potat.window_secs.saturating_add(SMALL_SLACK_SECS);
    let history = match fetch_with_retry(http, cfg, batch, potat, soft_cap, lifecycle).await {
        Ok(Some(history)) => history,
        Ok(None) => return false,
        Err(error) => {
            warn!(?error, "potat history fetch failed; skipping attribution");
            return !lifecycle.is_cancelled();
        }
    };
    if lifecycle.is_cancelled() {
        return false;
    }
    for event in &mut batch.events {
        attribute_one(event, &history, potat, soft_cap);
    }
    !lifecycle.is_cancelled()
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
#[expect(
    clippy::too_many_arguments,
    reason = "set switching coordinates fetched state, active catalog, websocket, and lifecycle"
)]
async fn handle_possible_set_switch(
    handle: &EmoteTrackerHandle,
    http: &reqwest::Client,
    cfg: &IngestConfig,
    gap_started_at: &mut Option<i64>,
    remembered_set_id: &mut String,
    sink: Option<&mut EventApiSink>,
    hinted_set_id: &str,
    lifecycle: &Lifecycle,
) -> Result<Option<()>> {
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    let snapshot = fetch_channel_set(http, &cfg.rest_base_url, &cfg.twitch_channel_id).await?;
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    match classify_set_id_mismatch(remembered_set_id, &snapshot.set_id) {
        SetIdMismatch::Foreign => {
            debug!(
                remembered_set_id = %remembered_set_id,
                hinted_set_id = %hinted_set_id,
                "ignoring EventAPI hint for non-active set"
            );
            Ok(Some(()))
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
            if lifecycle.is_cancelled() {
                return Ok(None);
            }
            let batch = reconcile_batch(at, &current, &snapshot.emotes, Some((gap_from, at)), None);
            handle.apply_set_changes(batch).await?;
            if lifecycle.is_cancelled() {
                return Ok(None);
            }

            if let Some(sink) = sink
                && resubscribe_emote_set(sink, remembered_set_id, &active_set_id, lifecycle)
                    .await?
                    .is_none()
            {
                return Ok(None);
            }

            *remembered_set_id = active_set_id;
            *gap_started_at = None;
            Ok(Some(()))
        }
    }
}

async fn resubscribe_emote_set(
    sink: &mut EventApiSink,
    old_set_id: &str,
    new_set_id: &str,
    lifecycle: &Lifecycle,
) -> Result<Option<()>> {
    if old_set_id == new_set_id {
        return Ok(Some(()));
    }
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    let unsub = unsubscribe_emote_set_update_message(old_set_id);
    if let Err(error) = sink.send(Message::Text(unsub.into())).await {
        warn!(?error, old_set_id, "failed to unsubscribe old emote set");
    }
    if lifecycle.is_cancelled() {
        return Ok(None);
    }
    let sub = subscribe_emote_set_update_message(new_set_id);
    sink.send(Message::Text(sub.into()))
        .await
        .wrap_err("failed to subscribe new emote set after switch")?;
    Ok(Some(()))
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
    Timestamp::now().as_millisecond()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::eventapi::ParsedEvent;
    use crate::emote_tracker::{DB_FILENAME, types::SetEventKind};
    use crate::settings::{PotatAttributionSettings, Settings};
    use rusqlite::Connection;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

    const POTAT_LOGIN: &str = "examplechannel";
    const POTAT_7TV_ID: &str = "01GNRG2MDG0000DM7N933JH49Y";

    fn rest_mock(channel_id: &str) -> MockBuilder {
        Mock::given(method("GET")).and(path(format!("/v3/users/twitch/{channel_id}")))
    }

    fn history_mock() -> MockBuilder {
        Mock::given(method("GET")).and(path(format!("/emotes/history/{POTAT_LOGIN}")))
    }

    /// Config pointing 7TV REST at `mock`.
    fn rest_cfg(mock: &MockServer, channel_id: &str) -> IngestConfig {
        IngestConfig {
            twitch_channel_id: channel_id.into(),
            twitch_channel_login: "testchannel".into(),
            rest_base_url: format!("{}/v3", mock.uri()),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: POTAT_HISTORY_BASE_URL.into(),
        }
    }

    /// Config pointing potat history at `mock`.
    fn potat_cfg(mock: &MockServer) -> IngestConfig {
        IngestConfig {
            twitch_channel_id: "123".into(),
            twitch_channel_login: POTAT_LOGIN.into(),
            rest_base_url: "https://7tv.io/v3".into(),
            eventapi_url: super::super::eventapi::EVENTAPI_URL.into(),
            potat_history_base_url: format!("{}/emotes/history", mock.uri()),
        }
    }

    fn batch_of(events: Vec<NewSetEvent>) -> SetChangeBatch {
        SetChangeBatch {
            updated_at: 1,
            mutations: vec![],
            events,
        }
    }

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
        rest_mock(channel_id)
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(DB_FILENAME);
        let handle = EmoteTrackerHandle::open(&db_path).unwrap();
        let cfg = rest_cfg(&mock, channel_id);

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

    #[tokio::test]
    async fn cancellation_before_startup_reconcile_prevents_rest_and_sqlite_effects() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        let channel_id = "12345";
        rest_mock(channel_id)
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(DB_FILENAME);
        let handle = EmoteTrackerHandle::open(&db_path).unwrap();
        let cfg = rest_cfg(&mock, channel_id);
        let lifecycle = Lifecycle::new();
        lifecycle.request_graceful_stop();

        let result =
            startup_reconcile_once_until(&handle, &reqwest::Client::new(), &cfg, None, &lifecycle)
                .await
                .unwrap();

        assert!(result.is_none());
        assert_eq!(count_events(&db_path, SetEventKind::Bootstrap), 0);
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
        rest_mock(channel_id)
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(DB_FILENAME);
        let handle = EmoteTrackerHandle::open(&db_path).unwrap();
        let cfg = rest_cfg(&mock, channel_id);

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
            &Lifecycle::new(),
        )
        .await
        .unwrap()
        .expect("running set-switch helper should complete");

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

        rest_mock(channel_id)
            .respond_with(ResponseTemplate::new(200).set_body_string(initial))
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        rest_mock(channel_id)
            .respond_with(ResponseTemplate::new(200).set_body_string(switched))
            .mount(&mock)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(DB_FILENAME);
        let handle = EmoteTrackerHandle::open(&db_path).unwrap();
        let cfg = rest_cfg(&mock, channel_id);

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
            &Lifecycle::new(),
        )
        .await
        .unwrap()
        .expect("running set-switch helper should complete");

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
            potat_7tv_user_id: POTAT_7TV_ID.into(),
        };
        SettingsHandle::from_settings(s)
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
        let body = include_str!("fixtures/potat_history_standard.json");
        history_mock()
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = potat_cfg(&mock);
        let settings = potat_settings_handle(true);
        let mut batch = batch_of(vec![add_event("e1", Some(POTAT_7TV_ID))]);

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
        history_mock()
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(0)
            .mount(&mock)
            .await;

        let cfg = potat_cfg(&mock);
        let settings = potat_settings_handle(false);
        let mut batch = batch_of(vec![add_event("e1", Some(POTAT_7TV_ID))]);

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert!(batch.events[0].requester_login.is_none());
        assert!(batch.events[0].via.is_none());
    }

    #[tokio::test]
    async fn multi_emote_batch_fetches_history_once() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
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
        history_mock()
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = potat_cfg(&mock);
        let settings = potat_settings_handle(true);
        let mut batch = batch_of(vec![
            add_event("e1", Some(POTAT_7TV_ID)),
            add_event("e2", Some(POTAT_7TV_ID)),
        ]);

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert_eq!(batch.events[0].requester_login.as_deref(), Some("alice"));
        assert_eq!(batch.events[1].requester_login.as_deref(), Some("bob"));
    }

    #[tokio::test]
    async fn soft_fail_leaves_attribution_null_on_http_error() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        history_mock()
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let cfg = potat_cfg(&mock);
        let settings = potat_settings_handle(true);
        let mut batch = batch_of(vec![add_event("e1", Some(POTAT_7TV_ID))]);

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert!(batch.events[0].requester_login.is_none());
    }

    #[tokio::test]
    async fn null_body_actor_value_actor_id_does_not_block_potat_attribution() {
        crate::install_crypto_provider();
        let raw = include_str!("fixtures/dispatch_push_null_actor_with_actor_id.json");
        let ParsedEvent::Dispatch(d) = crate::emote_tracker::eventapi::parse_server_message(raw)
            .unwrap()
            .unwrap()
        else {
            panic!("expected dispatch");
        };
        let mut batch = crate::emote_tracker::eventapi::live_change_to_batch(100, &d);
        assert!(
            batch.events[0].actor_7tv_id.is_none(),
            "true body.actor must stay null for Step A; value.actor_id must not poison the gate"
        );

        let emote_id = batch.events[0].emote_id.clone().unwrap();
        let mock = MockServer::start().await;
        let history = format!(
            r#"{{
              "data": [{{
                "history": [{{
                  "user_login": "EmoteFan",
                  "actor": "potatbotat",
                  "action": "ADD",
                  "type": "STANDARD",
                  "emote_id": "{emote_id}",
                  "provider": "STV",
                  "ago": "5s ago"
                }}]
              }}]
            }}"#
        );
        history_mock()
            .respond_with(ResponseTemplate::new(200).set_body_string(history))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = potat_cfg(&mock);
        let settings = potat_settings_handle(true);

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert!(batch.events[0].actor_7tv_id.is_none());
        assert_eq!(batch.events[0].requester_login.as_deref(), Some("emotefan"));
        assert_eq!(batch.events[0].via.as_deref(), Some("potatbotat"));
        assert_eq!(
            batch.events[0].attribution_method.as_deref(),
            Some("potat_history")
        );
    }

    #[tokio::test]
    async fn history_fetch_skipped_when_all_gates_permanently_closed() {
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        history_mock()
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(0)
            .mount(&mock)
            .await;

        let cfg = potat_cfg(&mock);
        let settings = potat_settings_handle(true);
        let mut batch = batch_of(vec![
            add_event("e1", Some("01HUMANEDITOR0000000000000")),
            add_event("e2", Some("01ANOTHERHUMAN000000000000")),
        ]);

        maybe_attribute_batch(&reqwest::Client::new(), &settings, &cfg, &mut batch).await;

        assert!(batch.events[0].requester_login.is_none());
        assert!(batch.events[1].requester_login.is_none());
    }

    #[tokio::test]
    async fn history_does_not_retry_when_gates_closed_after_first_fetch() {
        // Guard the fetch_with_retry path: if a fetch somehow runs for a
        // permanently closed batch, we must not sleep+retry. Empty response
        // would otherwise look like "no Step B hit" and trigger a second GET.
        crate::install_crypto_provider();
        let mock = MockServer::start().await;
        history_mock()
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"data":[{"history":[]}]}"#),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = potat_cfg(&mock);
        let potat = PotatAttributionSettings {
            enabled: true,
            window_secs: 15,
            potat_7tv_user_id: POTAT_7TV_ID.into(),
        };
        let soft_cap = potat.window_secs.saturating_add(SMALL_SLACK_SECS);
        let batch = batch_of(vec![add_event("e1", Some("01HUMANEDITOR0000000000000"))]);
        // Call fetch_with_retry directly so we exercise the no-retry branch
        // even though maybe_attribute_batch now skips the fetch entirely.
        let history = fetch_with_retry(
            &reqwest::Client::new(),
            &cfg,
            &batch,
            &potat,
            soft_cap,
            &Lifecycle::new(),
        )
        .await
        .unwrap()
        .expect("running fetch should complete");
        assert!(history.is_empty());
        // wiremock expect(1) asserts no second request on drop
    }
}
