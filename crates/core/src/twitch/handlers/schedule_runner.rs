//! Schedules engine.
//!
//! One `run_orchestrator` task subscribes to settings changes and owns every
//! per-schedule child through [`TaskGroup`]. A changed schedule is cancelled,
//! joined, and only then replaced. Cancellation is cooperative: an in-flight
//! `say()` is allowed to finish before its child is joined.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use eyre::WrapErr as _;
use futures_util::FutureExt;
use thiserror::Error;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, debug, info, info_span, warn};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::lifecycle::{Lifecycle, TaskGroup};
use crate::ping::{PingHandle, TriggerDecision};
use crate::schedule::{Schedule, ScheduleRuntimeStore};
use crate::settings::SettingsHandle;
use crate::twitch::ChatSender;
use crate::util::clock::Clock;

/// What a schedule needs to fire its optional ping: the ping actor, the
/// settings for the default cooldown, and the bot login used as `{sender}`.
#[derive(Clone)]
pub struct SchedulePing {
    pub handle: PingHandle,
    pub settings: SettingsHandle,
    pub bot_login: String,
}

impl SchedulePing {
    async fn fire<T, L>(&self, sender: &ChatSender<T, L>, channel: &str, ping_name: &str)
    where
        T: Transport,
        L: LoginCredentials,
    {
        let cooldown = Duration::from_secs(self.settings.load().pings.cooldown);
        // `public = true`: the bot is not a member of the ping it fires.
        match self
            .handle
            .try_record_trigger(ping_name.to_owned(), self.bot_login.clone(), cooldown, true)
            .await
        {
            TriggerDecision::Fire(rendered) => sender.say(channel.to_owned(), rendered).await,
            TriggerDecision::OnCooldown(remaining) => {
                debug!(ping = %ping_name, ?remaining, "scheduled ping on cooldown, skipped");
            }
            TriggerDecision::Skip => {
                warn!(ping = %ping_name, "scheduled ping missing or has no members, skipped");
            }
        }
    }
}

/// Why a schedule child stopped.
///
/// `Expired` and `Cancelled` are the only normal outcomes. The other variants
/// make an early return or panic visible to the parent runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScheduleCompletion {
    Expired,
    Cancelled,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "classification must recognize a future early return"
        )
    )]
    CompletedUnexpectedly,
    Panicked,
}

struct RunningSchedule {
    id: u64,
    captured: Schedule,
    tasks: TaskGroup<ScheduleCompletion>,
}

struct ScheduleCompletionEvent {
    id: u64,
    name: String,
}

#[derive(Debug, Error)]
pub(crate) enum ScheduleChildError {
    #[error("schedule child {name:?} panicked")]
    Panicked { name: String },

    #[error("schedule child {name:?} completed unexpectedly: {detail}")]
    CompletedUnexpectedly { name: String, detail: String },

    #[error("schedule child {name:?} failed to join ({reason})")]
    JoinFailed {
        name: String,
        reason: &'static str,
        #[source]
        source: JoinError,
    },
}

#[derive(Debug, Error)]
pub(crate) enum ScheduleOrchestratorError {
    #[error(transparent)]
    Child(#[from] ScheduleChildError),
}

enum ReconcileControl {
    Continue,
    Shutdown,
}

#[expect(
    clippy::too_many_arguments,
    reason = "orchestrator owns every dependency its schedule children need"
)]
pub async fn run_orchestrator<T, L>(
    sender: Arc<ChatSender<T, L>>,
    settings: SettingsHandle,
    runtime: Arc<ScheduleRuntimeStore>,
    channel: String,
    shutdown: Arc<Notify>,
    clock: Arc<dyn Clock>,
    lifecycle: Lifecycle,
    ping: SchedulePing,
) -> eyre::Result<()>
where
    T: Transport,
    L: LoginCredentials,
{
    try_run_orchestrator(
        sender, settings, runtime, channel, shutdown, clock, &lifecycle, &ping,
    )
    .await
    .wrap_err("scheduled-message child supervision failed")
}

/// Typed schedule orchestration seam. A child failure returns `Err`, which
/// [`run_orchestrator`] hands to the lifecycle supervisor as a Fault.
#[expect(
    clippy::too_many_arguments,
    reason = "orchestrator owns every dependency its schedule children need"
)]
pub(crate) async fn try_run_orchestrator<T, L>(
    sender: Arc<ChatSender<T, L>>,
    settings: SettingsHandle,
    runtime: Arc<ScheduleRuntimeStore>,
    channel: String,
    shutdown: Arc<Notify>,
    clock: Arc<dyn Clock>,
    lifecycle: &Lifecycle,
    ping: &SchedulePing,
) -> Result<(), ScheduleOrchestratorError>
where
    T: Transport,
    L: LoginCredentials,
{
    info!("Schedules orchestrator started");
    let mut running: HashMap<String, RunningSchedule> = HashMap::new();
    let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
    let mut next_child_id = 0_u64;

    // Keep one shutdown waiter armed for the whole orchestrator. This closes
    // the old `notify_waiters()` gap while reconciliation is awaiting a child
    // join: a shutdown arriving during that await remains observed afterward.
    let mut shutdown_request = Box::pin(shutdown.notified());
    shutdown_request.as_mut().enable();

    // Subscribe and arm shutdown before the initial reconcile so neither a
    // settings commit nor shutdown can be lost during initial child creation.
    let mut sub = settings.subscribe();
    let initial = reconcile(
        &sender,
        &settings,
        &runtime,
        &channel,
        &clock,
        &mut running,
        &mut next_child_id,
        &completion_tx,
        &mut shutdown_request,
        lifecycle,
        ping,
    )
    .await;
    let initial = match initial {
        Ok(control) => control,
        Err(error) => {
            let cleanup = stop_and_join_all(&mut running).await;
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(cleanup_error),
            };
        }
    };
    match initial {
        ReconcileControl::Continue => {}
        ReconcileControl::Shutdown => {
            stop_and_join_all(&mut running).await?;
            runtime.flush().await;
            return Ok(());
        }
    }

    loop {
        let control = tokio::select! {
            biased;
            () = &mut shutdown_request => ReconcileControl::Shutdown,
            () = sub.next() => {
                match reconcile(
                    &sender,
                    &settings,
                    &runtime,
                    &channel,
                    &clock,
                    &mut running,
                    &mut next_child_id,
                    &completion_tx,
                    &mut shutdown_request,
                    lifecycle,
                    ping,
                ).await {
                    Ok(ReconcileControl::Continue) => continue,
                    Ok(ReconcileControl::Shutdown) => ReconcileControl::Shutdown,
                    Err(error) => {
                        if let Err(stop_error) = stop_and_join_all(&mut running).await {
                            crate::log_handled!(warn, stop_error, "Schedules orchestrator: child cleanup failed after reconcile error");
                        }
                        return Err(error);
                    }
                }
            }
            event = completion_rx.recv() => {
                let Some(event) = event else {
                    if let Err(stop_error) = stop_and_join_all(&mut running).await {
                        crate::log_handled!(warn, stop_error, "Schedules orchestrator: child cleanup failed after completion channel closed");
                    }
                    return Err(ScheduleChildError::CompletedUnexpectedly {
                        name: "schedule child completion channel".to_owned(),
                        detail: "completion sender closed while the orchestrator was running".to_owned(),
                    }
                    .into());
                };
                if let Err(error) = observe_completed_child(&mut running, event).await {
                    if let Err(stop_error) = stop_and_join_all(&mut running).await {
                        crate::log_handled!(warn, stop_error, "Schedules orchestrator: child cleanup failed after child fault");
                    }
                    return Err(error);
                }
                continue;
            }
        };

        if matches!(control, ReconcileControl::Shutdown) {
            info!("Schedules orchestrator: shutdown");
            stop_and_join_all(&mut running).await?;
            runtime.flush().await;
            return Ok(());
        }
    }
}

async fn observe_completed_child(
    running: &mut HashMap<String, RunningSchedule>,
    event: ScheduleCompletionEvent,
) -> Result<(), ScheduleOrchestratorError> {
    // Completion events from a replaced generation can remain queued after the
    // old child was joined by reconciliation. They no longer own anything and
    // must not affect the replacement.
    if !running
        .get(&event.name)
        .is_some_and(|child| child.id == event.id)
    {
        return Ok(());
    }

    #[expect(
        clippy::expect_used,
        reason = "the event's child was checked present above"
    )]
    let mut child = running
        .remove(&event.name)
        .expect("completion event child was checked above");
    let completion = join_schedule_child(
        event.name.clone(),
        &mut child.tasks,
        /* cancellation_expected = */ false,
    )
    .await?;

    match completion {
        ScheduleCompletion::Expired => Ok(()),
        ScheduleCompletion::Cancelled | ScheduleCompletion::CompletedUnexpectedly => {
            Err(ScheduleChildError::CompletedUnexpectedly {
                name: event.name,
                detail: format!(
                    "child reported {completion:?} without an owner requesting cancellation"
                ),
            }
            .into())
        }
        ScheduleCompletion::Panicked => {
            Err(ScheduleChildError::Panicked { name: event.name }.into())
        }
    }
}

async fn stop_and_join_all(
    running: &mut HashMap<String, RunningSchedule>,
) -> Result<(), ScheduleOrchestratorError> {
    // Request cancellation for every child before awaiting any one of them,
    // then join all children even if one reports a fault.
    let children: Vec<(String, RunningSchedule)> = running.drain().collect();
    for (_, child) in &children {
        child.tasks.cancel();
    }

    let mut first_error = None;
    for (name, mut child) in children {
        if let Err(error) = join_schedule_child(
            name,
            &mut child.tasks,
            /* cancellation_expected = */ true,
        )
        .await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }

    match first_error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "reconcile needs cache, children, client, and lifecycle state"
)]
async fn reconcile<T, L, S>(
    sender: &Arc<ChatSender<T, L>>,
    settings: &SettingsHandle,
    runtime: &Arc<ScheduleRuntimeStore>,
    channel: &str,
    clock: &Arc<dyn Clock>,
    running: &mut HashMap<String, RunningSchedule>,
    next_child_id: &mut u64,
    completion_tx: &mpsc::UnboundedSender<ScheduleCompletionEvent>,
    shutdown: &mut S,
    lifecycle: &Lifecycle,
    ping: &SchedulePing,
) -> Result<ReconcileControl, ScheduleOrchestratorError>
where
    T: Transport,
    L: LoginCredentials,
    S: Future<Output = ()> + Unpin,
{
    if shutdown_is_ready(shutdown).await {
        return Ok(ReconcileControl::Shutdown);
    }

    let desired: HashMap<String, Schedule> = settings
        .load()
        .schedules
        .iter()
        .filter(|schedule| schedule.enabled)
        .map(|schedule| (schedule.name.clone(), schedule.clone()))
        .collect();

    // Build the replacement list first. Changed/disabled schedules are joined
    // one at a time before any replacement is created.
    let stopped_names: Vec<String> = running
        .iter()
        .filter_map(|(name, child)| {
            let should_stop = desired.get(name).is_none_or(|want| want != &child.captured);
            should_stop.then(|| name.clone())
        })
        .collect();

    let mut first_error = None;
    for name in stopped_names {
        let Some(mut child) = running.remove(&name) else {
            continue;
        };
        child.tasks.cancel();

        let joined = {
            let mut join = Box::pin(child.tasks.join_all());
            tokio::select! {
                biased;
                () = &mut *shutdown => {
                    drop(join);
                    // Keep ownership in the map. The shutdown path will
                    // request cancellation again and join this child.
                    running.insert(name, child);
                    return Ok(ReconcileControl::Shutdown);
                }
                completions = &mut join => completions,
            }
        };

        if let Err(error) = classify_schedule_completions(&name, joined, true)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }

    if let Some(error) = first_error {
        return Err(error.into());
    }
    if shutdown_is_ready(shutdown).await {
        return Ok(ReconcileControl::Shutdown);
    }

    for (name, schedule) in desired {
        if running.contains_key(&name) {
            continue;
        }
        // Operation boundary: never accept replacement work after shutdown.
        if shutdown_is_ready(shutdown).await {
            return Ok(ReconcileControl::Shutdown);
        }

        let id = *next_child_id;
        #[expect(
            clippy::expect_used,
            reason = "a u64 generation counter cannot be exhausted in practice"
        )]
        let next_id = next_child_id
            .checked_add(1)
            .expect("schedule child generation counter exhausted");
        *next_child_id = next_id;
        let child = spawn_schedule_child(
            id,
            name.clone(),
            schedule.clone(),
            sender.clone(),
            runtime.clone(),
            channel.to_owned(),
            clock.clone(),
            completion_tx.clone(),
            lifecycle,
            ping.clone(),
        )
        .await;
        running.insert(name, child);
    }

    Ok(ReconcileControl::Continue)
}

async fn shutdown_is_ready<S>(shutdown: &mut S) -> bool
where
    S: Future<Output = ()> + Unpin,
{
    matches!(
        std::future::poll_fn(|cx| Poll::Ready(Future::poll(Pin::new(shutdown), cx))).await,
        Poll::Ready(())
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "child task owns its schedule, client, and shutdown signals"
)]
async fn spawn_schedule_child<T, L>(
    id: u64,
    name: String,
    schedule: Schedule,
    sender: Arc<ChatSender<T, L>>,
    runtime: Arc<ScheduleRuntimeStore>,
    channel: String,
    clock: Arc<dyn Clock>,
    completion_tx: mpsc::UnboundedSender<ScheduleCompletionEvent>,
    lifecycle: &Lifecycle,
    ping: SchedulePing,
) -> RunningSchedule
where
    T: Transport,
    L: LoginCredentials,
{
    let event_name = name.clone();
    let child_schedule = schedule.clone();
    let mut tasks = TaskGroup::new(lifecycle, format!("schedule child {name}"));
    let child_cancellation = tasks.cancellation_token();

    tasks
        .spawn(name, async move {
            let completion = AssertUnwindSafe(run_schedule_task(
                child_schedule,
                sender,
                runtime,
                channel,
                clock,
                child_cancellation,
                ping,
            ))
            .catch_unwind()
            .await
            .unwrap_or(ScheduleCompletion::Panicked);

            // This channel is wake-up/classification only. The TaskGroup remains
            // the owner and is joined before the result is acted upon.
            completion_tx
                .send(ScheduleCompletionEvent {
                    id,
                    name: event_name,
                })
                .ok();
            completion
        })
        .await;

    RunningSchedule {
        id,
        captured: schedule,
        tasks,
    }
}

/// Join exactly one owned child and classify its result. A `JoinError` is never
/// discarded; cancellation and panic remain distinguishable to the parent.
async fn join_schedule_child(
    name: String,
    tasks: &mut TaskGroup<ScheduleCompletion>,
    cancellation_expected: bool,
) -> Result<ScheduleCompletion, ScheduleChildError> {
    let completions = tasks.join_all().await;
    if completions.len() != 1 {
        return Err(ScheduleChildError::CompletedUnexpectedly {
            name,
            detail: format!(
                "owned child group produced {} completion records; expected exactly one",
                completions.len()
            ),
        });
    }

    #[expect(
        clippy::expect_used,
        reason = "exactly one completion was checked above"
    )]
    let completion = completions
        .into_iter()
        .next()
        .expect("completion count was checked above")
        .into_result()
        .map_err(|source| {
            let reason = if source.is_panic() {
                "panicked"
            } else if source.is_cancelled() {
                "was cancelled without a joined cooperative completion"
            } else {
                "join failed"
            };
            ScheduleChildError::JoinFailed {
                name: name.clone(),
                reason,
                source,
            }
        })?;

    match completion {
        ScheduleCompletion::Expired => Ok(ScheduleCompletion::Expired),
        ScheduleCompletion::Cancelled if cancellation_expected => Ok(ScheduleCompletion::Cancelled),
        ScheduleCompletion::Cancelled => Err(ScheduleChildError::CompletedUnexpectedly {
            name,
            detail: "child reported Cancelled without an owner requesting cancellation".to_owned(),
        }),
        ScheduleCompletion::Panicked => Err(ScheduleChildError::Panicked { name }),
        ScheduleCompletion::CompletedUnexpectedly => {
            Err(ScheduleChildError::CompletedUnexpectedly {
                name,
                detail: "child returned without Expiry or cancellation".to_owned(),
            })
        }
    }
}

fn classify_schedule_completions(
    name: &str,
    completions: Vec<crate::lifecycle::TaskCompletion<ScheduleCompletion>>,
    cancellation_expected: bool,
) -> Result<ScheduleCompletion, ScheduleChildError> {
    if completions.len() != 1 {
        return Err(ScheduleChildError::CompletedUnexpectedly {
            name: name.to_owned(),
            detail: format!(
                "owned child group produced {} completion records; expected exactly one",
                completions.len()
            ),
        });
    }

    #[expect(
        clippy::expect_used,
        reason = "exactly one completion was checked above"
    )]
    let completion = completions
        .into_iter()
        .next()
        .expect("completion count was checked above")
        .into_result()
        .map_err(|source| ScheduleChildError::JoinFailed {
            name: name.to_owned(),
            reason: if source.is_panic() {
                "panicked"
            } else if source.is_cancelled() {
                "was cancelled without a joined cooperative completion"
            } else {
                "join failed"
            },
            source,
        })?;

    match completion {
        ScheduleCompletion::Expired => Ok(ScheduleCompletion::Expired),
        ScheduleCompletion::Cancelled if cancellation_expected => Ok(ScheduleCompletion::Cancelled),
        ScheduleCompletion::Cancelled => Err(ScheduleChildError::CompletedUnexpectedly {
            name: name.to_owned(),
            detail: "child reported Cancelled without an owner requesting cancellation".to_owned(),
        }),
        ScheduleCompletion::Panicked => Err(ScheduleChildError::Panicked {
            name: name.to_owned(),
        }),
        ScheduleCompletion::CompletedUnexpectedly => {
            Err(ScheduleChildError::CompletedUnexpectedly {
                name: name.to_owned(),
                detail: "child returned without Expiry or cancellation".to_owned(),
            })
        }
    }
}

async fn run_schedule_task<T, L>(
    schedule: Schedule,
    sender: Arc<ChatSender<T, L>>,
    runtime: Arc<ScheduleRuntimeStore>,
    channel: String,
    clock: Arc<dyn Clock>,
    cancel: CancellationToken,
    ping: SchedulePing,
) -> ScheduleCompletion
where
    T: Transport,
    L: LoginCredentials,
{
    loop {
        let now_berlin = crate::util::to_berlin(clock.now_utc());
        let Some(next_berlin) = schedule.next_active_fire(now_berlin) else {
            info!(bot.schedule.name = %schedule.name, "no further fire, exiting");
            return ScheduleCompletion::Expired;
        };
        let next_utc = next_berlin.timestamp();
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                info!(bot.schedule.name = %schedule.name, "cancelled, exiting");
                return ScheduleCompletion::Cancelled;
            }
            () = clock.sleep_until(next_utc) => {}
        }
        // Re-check cancellation after wakeup: select may choose the sleep
        // branch even if cancellation became ready at the same instant.
        if cancel.is_cancelled() {
            info!(bot.schedule.name = %schedule.name, "cancelled after wakeup, exiting");
            return ScheduleCompletion::Cancelled;
        }

        // One root trace per fire; the sleep above stays outside it.
        let fire_span = info_span!(
            parent: None,
            "schedule.fire",
            bot.schedule.name = %schedule.name,
            ping.name = schedule.ping.as_deref(),
        );
        async {
            // Operation-boundary policy: cancellation is checked immediately
            // before the externally visible send. If cancellation races after this
            // check, the atomic `say()` is allowed to finish before joining.
            sender.say(channel.clone(), schedule.message.clone()).await;
            // The send completed, so record the fire before observing cancellation.
            // A cancellation racing this await must not make a successful send
            // disappear from the runtime counters.
            runtime.record_fire(&schedule.name, clock.now_utc()).await;
            if let Some(ping_name) = &schedule.ping {
                ping.fire(&sender, &channel, ping_name).await;
            }
        }
        .instrument(fire_span)
        .await;
        if cancel.is_cancelled() {
            info!(bot.schedule.name = %schedule.name, "cancelled after send, exiting");
            return ScheduleCompletion::Cancelled;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    use super::{
        ScheduleChildError, ScheduleCompletion, join_schedule_child, shutdown_is_ready,
        stop_and_join_all,
    };
    use crate::lifecycle::{Lifecycle, TaskGroup};

    async fn completion_task(completion: ScheduleCompletion) -> TaskGroup<ScheduleCompletion> {
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "test schedule child");
        group.spawn("child", async move { completion }).await;
        group
    }

    #[tokio::test]
    async fn explicit_expiry_and_requested_cancellation_are_normal() {
        let mut expired = completion_task(ScheduleCompletion::Expired).await;
        assert_eq!(
            join_schedule_child("expired".into(), &mut expired, false)
                .await
                .unwrap(),
            ScheduleCompletion::Expired
        );

        let mut cancelled = completion_task(ScheduleCompletion::Cancelled).await;
        assert_eq!(
            join_schedule_child("cancelled".into(), &mut cancelled, true)
                .await
                .unwrap(),
            ScheduleCompletion::Cancelled
        );
    }

    #[tokio::test]
    async fn unexpected_completion_and_reported_panic_are_faults() {
        let mut unexpected = completion_task(ScheduleCompletion::CompletedUnexpectedly).await;
        assert!(matches!(
            join_schedule_child("unexpected".into(), &mut unexpected, false).await,
            Err(ScheduleChildError::CompletedUnexpectedly { .. })
        ));

        let mut panicked = completion_task(ScheduleCompletion::Panicked).await;
        assert!(matches!(
            join_schedule_child("panicked".into(), &mut panicked, false).await,
            Err(ScheduleChildError::Panicked { .. })
        ));
    }

    #[tokio::test]
    async fn join_error_is_propagated_with_its_panic_classification() {
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "panicking schedule child");
        group
            .spawn("child", async { panic!("poll child failed") })
            .await;

        let error = join_schedule_child("panic".into(), &mut group, false)
            .await
            .expect_err("join error must be a child fault");

        let ScheduleChildError::JoinFailed { source, .. } = error else {
            panic!("expected a preserved JoinError");
        };
        assert!(source.is_panic());
    }

    #[tokio::test]
    async fn replacement_join_waits_for_cooperative_cancellation() {
        let cancellation = CancellationToken::new();
        let child_cancellation = cancellation.clone();
        let (release_tx, release_rx) = oneshot::channel();
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "replacement child");
        group
            .spawn("old", async move {
                child_cancellation.cancelled().await;
                release_rx.await.expect("test releases child");
                ScheduleCompletion::Cancelled
            })
            .await;
        cancellation.cancel();

        let joiner =
            tokio::spawn(async move { join_schedule_child("old".into(), &mut group, true).await });
        tokio::task::yield_now().await;
        assert!(!joiner.is_finished(), "replacement must wait for old child");

        release_tx.send(()).unwrap();
        assert_eq!(
            joiner.await.unwrap().unwrap(),
            ScheduleCompletion::Cancelled
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_then_joins_every_child() {
        let lifecycle = Lifecycle::new();
        let mut running = HashMap::new();
        for (index, completion) in [ScheduleCompletion::Expired, ScheduleCompletion::Cancelled]
            .into_iter()
            .enumerate()
        {
            let mut tasks = TaskGroup::new(&lifecycle, format!("test child {index}"));
            let child_cancellation = tasks.cancellation_token();
            tasks
                .spawn(format!("child-{index}"), async move {
                    if matches!(completion, ScheduleCompletion::Cancelled) {
                        child_cancellation.cancelled().await;
                    }
                    completion
                })
                .await;
            running.insert(
                format!("schedule-{index}"),
                super::RunningSchedule {
                    id: index as u64,
                    captured: crate::schedule::Schedule {
                        ping: None,
                        name: format!("schedule-{index}"),
                        message: String::new(),
                        trigger: crate::schedule::Trigger::Interval {
                            every: Duration::from_secs(60),
                            days: crate::schedule::WeekdaySet::default(),
                            active_from: None,
                            active_to: None,
                        },
                        start_date: None,
                        end_date: None,
                        enabled: true,
                    },
                    tasks,
                },
            );
        }

        stop_and_join_all(&mut running).await.unwrap();

        assert!(running.is_empty());
    }

    #[tokio::test]
    async fn shutdown_readiness_does_not_consume_a_pending_signal() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let mut waiter = Box::pin(notify.notified());
        waiter.as_mut().enable();

        assert!(!shutdown_is_ready(&mut waiter).await);
        notify.notify_waiters();
        assert!(shutdown_is_ready(&mut waiter).await);
    }
}
