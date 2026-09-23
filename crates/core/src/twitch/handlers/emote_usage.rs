//! Record per-token emote usage from main-channel PRIVMSGs into SQLite.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinError;
use tokio::time::{MissedTickBehavior, interval};
use tracing::error;
use twitch_irc::message::ServerMessage;

use crate::emote_tracker::{EmoteRow, EmoteTrackerHandle, NewUsageEvent, match_usage_tokens};
use crate::lifecycle::{Lifecycle, TaskGroup};
use crate::settings::SettingsHandle;
use crate::twitch::clanker::should_flag_usage;
use crate::twitch::echo::EchoGuard;

pub const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

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

async fn refresh_catalog(handle: &EmoteTrackerHandle, out: &mut HashMap<String, String>) {
    match handle.list_active_emotes().await {
        Ok(rows) => *out = active_name_index(&rows),
        Err(error) => error!(?error, "emote usage catalog refresh failed"),
    }
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

fn spawn_usage_writer(
    emote_tracker: Arc<EmoteTrackerHandle>,
) -> (
    TaskGroup<UsageIngestCompletion>,
    mpsc::UnboundedSender<Vec<NewUsageEvent>>,
    mpsc::UnboundedReceiver<UsageIngestCompletion>,
) {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (completion_tx, completion_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskGroup::new("emote usage SQLite writer");
    let cancellation = tasks.cancellation_token();
    tasks.spawn("usage writer", async move {
        let handle = emote_tracker.clone();
        let completion = AssertUnwindSafe(run_usage_ingest(events_rx, cancellation, |events| {
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
        let _ = completion_tx.send(completion);
        completion
    });
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
    classify_usage_ingest_completion(completion, cancellation_expected)
        .expect_err("an unsolicited usage-writer completion is a child fault")
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

pub async fn run_emote_usage_handler(
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
    emote_tracker: Arc<EmoteTrackerHandle>,
    settings: SettingsHandle,
    channel: String,
    bot_username: String,
    echo: Arc<EchoGuard>,
    lifecycle: Lifecycle,
) {
    let mut active_names = HashMap::new();
    refresh_catalog(&emote_tracker, &mut active_names).await;

    let mut refresh = interval(CATALOG_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Mouth logins never change post-startup; clone once so the per-message
    // deny-list merge below is just a cheap chain, not a re-clone of the guard's set.
    let mouth_logins: Vec<String> = echo.mouth_logins().iter().cloned().collect();
    let (mut usage_tasks, usage_tx, mut usage_completion_rx) =
        spawn_usage_writer(emote_tracker.clone());

    loop {
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => {
                stop_usage_writer(&mut usage_tasks)
                    .await
                    .unwrap_or_else(|error| {
                        panic!("emote usage writer child failed: {error}")
                    });
                return;
            }
            completion = usage_completion_rx.recv() => {
                let error = fail_on_usage_writer_completion(&mut usage_tasks, completion).await;
                panic!("emote usage writer child failed: {error}");
            }
            _ = refresh.tick() => {
                if lifecycle.is_cancelled() {
                    continue;
                }
                refresh_catalog(&emote_tracker, &mut active_names).await;
            }
            msg = broadcast_rx.recv() => {
                match msg {
                    Ok(ServerMessage::Privmsg(p)) if p.channel_login == channel => {
                        let user_id = p.sender.id.as_str();
                        if user_id.is_empty() {
                            continue;
                        }
                        let emote_ids = match_usage_tokens(&p.message_text, &active_names);
                        if emote_ids.is_empty() {
                            continue;
                        }
                        let snapshot = settings.load();
                        let deny_with_mouths: Vec<String> = snapshot
                            .twitch
                            .emote_usage_deny_list
                            .iter()
                            .cloned()
                            .chain(mouth_logins.iter().cloned())
                            .collect();
                        let flagged =
                            should_flag_usage(&p.sender.login, &bot_username, &deny_with_mouths);
                        let at = p.server_timestamp.timestamp_millis();
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
                        if lifecycle.is_cancelled() {
                            continue;
                        }
                        if usage_tx.send(events).is_err() {
                            let error =
                                fail_on_usage_writer_completion(&mut usage_tasks, None).await;
                            panic!("emote usage writer child failed: {error}");
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => {
                        stop_usage_writer(&mut usage_tasks)
                            .await
                            .unwrap_or_else(|error| {
                                panic!("emote usage writer child failed: {error}")
                            });
                        return;
                    }
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

    #[tokio::test]
    async fn cancellation_before_write_prevents_new_sqlite_work() {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let inserted = Arc::new(AtomicUsize::new(0));
        let mut tasks = TaskGroup::new("test usage writer");
        let cancellation = tasks.cancellation_token();
        let task_inserted = inserted.clone();
        tasks.spawn("usage writer", async move {
            let inserted = task_inserted.clone();
            run_usage_ingest(events_rx, cancellation, move |_events| {
                let inserted = inserted.clone();
                async move {
                    inserted.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
        });
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
        let mut tasks = TaskGroup::new("panicking usage writer");
        tasks.spawn("usage writer", async { panic!("SQLite writer failed") });

        let error = stop_usage_writer(&mut tasks)
            .await
            .expect_err("writer panic must fault the parent");

        let UsageIngestError::JoinFailed { source } = error else {
            panic!("expected the original JoinError");
        };
        assert!(source.is_panic());
    }
}
