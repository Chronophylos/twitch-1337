//! Record per-token emote usage from main-channel PRIVMSGs into SQLite.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use eyre::WrapErr as _;
use futures_util::FutureExt;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinError;
use tokio::time::{MissedTickBehavior, interval};
use tracing::error;
use twitch_irc::message::ServerMessage;

use crate::emote_tracker::{
    EmoteRow, EmoteTrackerHandle, NewUsageEvent, fetch_global_set, match_usage_tokens,
};
use crate::lifecycle::{Lifecycle, TaskGroup};
use crate::settings::SettingsHandle;
use crate::twitch::clanker::should_flag_usage;
use crate::twitch::handlers::Inbound;

pub const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const GLOBAL_CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
const DEFAULT_API_BASE: &str = "https://7tv.io/v3";

const USAGE_WRITER_FAILED: &str = "emote usage writer child failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageIngestCompletion {
    Cancelled,
    InputClosed,
    Panicked,
}

#[derive(Debug, Error)]
enum UsageIngestError {
    #[error("emote usage writer completed unexpectedly: {completion:?}")]
    CompletedUnexpectedly { completion: UsageIngestCompletion },

    #[error("emote usage writer panicked")]
    Panicked,

    #[error("emote usage writer failed to join: {source}")]
    JoinFailed {
        #[source]
        source: JoinError,
    },
}

pub(crate) fn active_name_index(rows: &[EmoteRow]) -> HashMap<String, String> {
    let mut sorted: Vec<_> = rows.iter().filter(|r| r.active).collect();
    sorted.sort_by_key(|r| &r.emote_id);
    let mut out = HashMap::new();
    for row in sorted {
        out.entry(row.name.clone())
            .or_insert_with(|| row.emote_id.clone());
    }
    out
}

async fn refresh_catalog(
    handle: &EmoteTrackerHandle,
    out: &mut HashMap<String, String>,
    globals: &HashMap<String, String>,
) {
    match handle.list_active_emotes().await {
        Ok(rows) => {
            *out = active_name_index(&rows);
            // Channel membership wins a same-name collision with a global.
            for (name, id) in globals {
                out.entry(name.clone()).or_insert_with(|| id.clone());
            }
        }
        Err(error) => error!(?error, "emote usage catalog refresh failed"),
    }
}

fn global_name_index(rows: &[crate::emote_tracker::GlobalCatalogEmote]) -> HashMap<String, String> {
    let mut sorted: Vec<_> = rows.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let mut out = HashMap::new();
    for emote in sorted {
        out.entry(emote.name.clone())
            .or_insert_with(|| emote.id.clone());
    }
    out
}

async fn run_usage_ingest<Insert, Fut>(
    mut events_rx: mpsc::UnboundedReceiver<Vec<NewUsageEvent>>,
    cancellation: tokio_util::sync::CancellationToken,
    mut insert: Insert,
) -> UsageIngestCompletion
where
    Insert: FnMut(Vec<NewUsageEvent>) -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        let events = tokio::select! {
            biased;
            () = cancellation.cancelled() => return UsageIngestCompletion::Cancelled,
            events = events_rx.recv() => {
                let Some(events) = events else {
                    return UsageIngestCompletion::InputClosed;
                };
                events
            }
        };

        // Operation boundary: do not start a new SQLite write after
        // cancellation. If cancellation races after this check, the started
        // atomic insertion is allowed to finish before the owner joins it.
        if cancellation.is_cancelled() {
            return UsageIngestCompletion::Cancelled;
        }
        insert(events).await;
    }
}

async fn spawn_usage_writer(
    emote_tracker: Arc<EmoteTrackerHandle>,
    lifecycle: &Lifecycle,
) -> (
    TaskGroup<UsageIngestCompletion>,
    mpsc::UnboundedSender<Vec<NewUsageEvent>>,
    mpsc::UnboundedReceiver<UsageIngestCompletion>,
) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (completion_tx, completion_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskGroup::new(lifecycle, "emote usage SQLite writer");
    let cancellation = tasks.cancellation_token();
    tasks
        .spawn("usage writer", async move {
            let handle = emote_tracker.clone();
            let completion =
                AssertUnwindSafe(run_usage_ingest(events_rx, cancellation, |events| {
                    let handle = handle.clone();
                    async move {
                        if let Err(error) = handle.insert_usage_events(events).await {
                            error!(?error, "emote usage insert failed");
                        }
                    }
                }))
                .catch_unwind()
                .await
                .unwrap_or(UsageIngestCompletion::Panicked);
            completion_tx.send(completion).ok();
            completion
        })
        .await;
    (tasks, events_tx, completion_rx)
}

async fn stop_usage_writer(
    tasks: &mut TaskGroup<UsageIngestCompletion>,
) -> Result<(), UsageIngestError> {
    tasks.cancel();
    let mut completions = tasks.join_all().await;
    if completions.len() != 1 {
        return Err(UsageIngestError::CompletedUnexpectedly {
            completion: UsageIngestCompletion::Panicked,
        });
    }
    let completion = completions
        .remove(0)
        .into_result()
        .map_err(|source| UsageIngestError::JoinFailed { source })?;
    classify_usage_ingest_completion(completion, /* cancellation_expected = */ true)
}

async fn fail_on_usage_writer_completion(
    tasks: &mut TaskGroup<UsageIngestCompletion>,
    observed: Option<UsageIngestCompletion>,
) -> UsageIngestError {
    let cancellation_expected = observed.is_none();
    if cancellation_expected {
        tasks.cancel();
    }
    let mut completions = tasks.join_all().await;
    if completions.len() != 1 {
        return UsageIngestError::CompletedUnexpectedly {
            completion: observed.unwrap_or(UsageIngestCompletion::Panicked),
        };
    }
    let completion = match completions.remove(0).into_result() {
        Ok(completion) => completion,
        Err(source) => return UsageIngestError::JoinFailed { source },
    };
    if let Some(observed) = observed
        && observed != completion
    {
        return UsageIngestError::CompletedUnexpectedly {
            completion: observed,
        };
    }
    // An unsolicited usage-writer completion is a child fault even when the
    // writer reported a clean cancellation.
    match classify_usage_ingest_completion(completion, cancellation_expected) {
        Err(error) => error,
        Ok(()) => UsageIngestError::CompletedUnexpectedly { completion },
    }
}

fn classify_usage_ingest_completion(
    completion: UsageIngestCompletion,
    cancellation_expected: bool,
) -> Result<(), UsageIngestError> {
    match completion {
        UsageIngestCompletion::Cancelled if cancellation_expected => Ok(()),
        UsageIngestCompletion::Cancelled | UsageIngestCompletion::InputClosed => {
            Err(UsageIngestError::CompletedUnexpectedly { completion })
        }
        UsageIngestCompletion::Panicked => Err(UsageIngestError::Panicked),
    }
}

/// Returns `Err` when the SQLite usage-writer child fails, which Faults the
/// runtime.
pub async fn run_emote_usage_handler(
    mut broadcast_rx: broadcast::Receiver<Inbound>,
    emote_tracker: Arc<EmoteTrackerHandle>,
    settings: SettingsHandle,
    channel: String,
    bot_username: String,
    lifecycle: Lifecycle,
) -> eyre::Result<()> {
    let mut active_names = HashMap::new();
    let mut global_names = HashMap::new();
    refresh_catalog(&emote_tracker, &mut active_names, &global_names).await;

    let mut refresh = interval(CATALOG_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut global_refresh = interval(GLOBAL_CATALOG_REFRESH_INTERVAL);
    global_refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let global_http = reqwest::Client::builder()
        .user_agent(crate::APP_USER_AGENT)
        .timeout(crate::emote_tracker::INGEST_HTTP_TIMEOUT)
        .build()
        .wrap_err("build global emote usage HTTP client")?;

    let (mut usage_tasks, usage_tx, mut usage_completion_rx) =
        spawn_usage_writer(emote_tracker.clone(), &lifecycle).await;

    loop {
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => {
                stop_usage_writer(&mut usage_tasks)
                    .await
                    .wrap_err(USAGE_WRITER_FAILED)?;
                return Ok(());
            }
            completion = usage_completion_rx.recv() => {
                let error = fail_on_usage_writer_completion(&mut usage_tasks, completion).await;
                return Err(eyre::Report::new(error).wrap_err(USAGE_WRITER_FAILED));
            }
            _ = global_refresh.tick() => {
                if lifecycle.is_cancelled() {
                    continue;
                }
                let snapshot = settings.load();
                let enabled = snapshot.ai.emotes_agent.enabled;
                let base_url = snapshot
                    .ai
                    .emotes
                    .as_ref()
                    .and_then(|cfg| cfg.base_url.clone())
                    .unwrap_or_else(|| DEFAULT_API_BASE.into());
                drop(snapshot);
                if enabled {
                    match fetch_global_set(&global_http, &base_url).await {
                        Ok(rows) => {
                            let names = global_name_index(&rows);
                            if let Err(error) = emote_tracker
                                .ensure_global_emotes(rows, jiff::Timestamp::now().as_millisecond())
                                .await
                            {
                                crate::log_handled!(error, error, "global emote usage FK catalog write failed");
                            } else {
                                global_names = names;
                                refresh_catalog(&emote_tracker, &mut active_names, &global_names).await;
                            }
                        }
                        Err(error) => {
                            crate::log_handled!(error, error, "global emote usage catalog refresh failed");
                        }
                    }
                } else if !global_names.is_empty() {
                    global_names.clear();
                    refresh_catalog(&emote_tracker, &mut active_names, &global_names).await;
                }
            }
            _ = refresh.tick() => {
                if lifecycle.is_cancelled() {
                    continue;
                }
                refresh_catalog(&emote_tracker, &mut active_names, &global_names).await;
            }
            msg = broadcast_rx.recv() => {
                let inbound = match msg {
                    Ok(inbound) => inbound,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        crate::util::observability::metrics::broadcast_lagged("emote_usage", skipped);
                        continue;
                    }
                    Err(_) => {
                        stop_usage_writer(&mut usage_tasks)
                            .await
                            .wrap_err(USAGE_WRITER_FAILED)?;
                        return Ok(());
                    }
                };
                let ServerMessage::Privmsg(p) = &inbound.msg else {
                    continue;
                };
                if p.channel_login != channel {
                    continue;
                }
                let user_id = p.sender.id.as_str();
                if user_id.is_empty() {
                    continue;
                }
                let emote_ids = match_usage_tokens(&p.message_text, &active_names);
                if emote_ids.is_empty() || lifecycle.is_cancelled() {
                    continue;
                }
                let origin = inbound.origin().await;
                let sent = inbound.handler_span("emote_usage").in_scope(|| {
                    let snapshot = settings.load();
                    let flagged = origin.flags_emote_usage()
                        || should_flag_usage(
                            &p.sender.login,
                            &bot_username,
                            &snapshot.twitch.emote_usage_deny_list,
                        );
                    let at = p.server_timestamp.as_millisecond();
                    let login = Some(p.sender.login.clone());
                    let events: Vec<NewUsageEvent> = emote_ids
                        .into_iter()
                        .map(|emote_id| NewUsageEvent {
                            at,
                            emote_id,
                            twitch_user_id: user_id.to_owned(),
                            login: login.clone(),
                            flagged,
                        })
                        .collect();
                    usage_tx.send(events).is_ok()
                });
                if !sent {
                    let error = fail_on_usage_writer_completion(&mut usage_tasks, None).await;
                    return Err(eyre::Report::new(error).wrap_err(USAGE_WRITER_FAILED));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn active_name_index_lowest_id_wins_collision() {
        let rows = vec![
            EmoteRow {
                emote_id: "b".into(),
                name: "Same".into(),
                active: true,
                updated_at: 0,
            },
            EmoteRow {
                emote_id: "a".into(),
                name: "Same".into(),
                active: true,
                updated_at: 0,
            },
        ];
        let map = active_name_index(&rows);
        assert_eq!(map.get("Same").unwrap(), "a");
    }

    #[test]
    fn global_name_index_lowest_id_wins_collision() {
        let rows = vec![
            crate::emote_tracker::GlobalCatalogEmote {
                id: "b".into(),
                name: "Same".into(),
            },
            crate::emote_tracker::GlobalCatalogEmote {
                id: "a".into(),
                name: "Same".into(),
            },
        ];
        assert_eq!(global_name_index(&rows).get("Same").unwrap(), "a");
    }

    #[tokio::test]
    async fn cancellation_before_write_prevents_new_sqlite_work() {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let inserted = Arc::new(AtomicUsize::new(0));
        let lifecycle = Lifecycle::new();
        let mut tasks = TaskGroup::new(&lifecycle, "test usage writer");
        let cancellation = tasks.cancellation_token();
        let task_inserted = inserted.clone();
        tasks
            .spawn("usage writer", async move {
                let inserted = task_inserted.clone();
                run_usage_ingest(events_rx, cancellation, move |_events| {
                    let inserted = inserted.clone();
                    async move {
                        inserted.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .await
            })
            .await;
        events_tx
            .send(vec![NewUsageEvent {
                at: 0,
                emote_id: "emote".to_owned(),
                twitch_user_id: "user".to_owned(),
                login: Some("login".to_owned()),
                flagged: false,
            }])
            .unwrap();
        tasks.cancel();

        let completions = tasks.join_all().await;

        assert_eq!(
            completions[0]
                .result()
                .as_ref()
                .expect("cooperative cancellation should be joined"),
            &UsageIngestCompletion::Cancelled
        );
        assert_eq!(inserted.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn usage_writer_join_error_is_preserved() {
        let lifecycle = Lifecycle::new();
        let mut tasks = TaskGroup::new(&lifecycle, "panicking usage writer");
        tasks
            .spawn("usage writer", async { panic!("SQLite writer failed") })
            .await;

        let error = stop_usage_writer(&mut tasks)
            .await
            .expect_err("writer panic must fault the parent");

        let UsageIngestError::JoinFailed { source } = error else {
            panic!("expected the original JoinError");
        };
        assert!(source.is_panic());
    }
}
