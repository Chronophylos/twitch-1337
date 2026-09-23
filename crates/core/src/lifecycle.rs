//! Latched application lifecycle state.
//!
//! This module deliberately contains no domain policy. It only gives
//! application-owned tasks one cheap, cloneable way to observe and request
//! runtime shutdown without missing an earlier request.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::{
    sync::watch,
    task::{JoinError, JoinHandle},
};
use tokio_util::sync::CancellationToken;

/// The process lifecycle state shared by all application-owned tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// The runtime is accepting work.
    Running,
    /// An intentional shutdown was requested.
    GracefulStop,
    /// A task, signal adapter, or shutdown request failed the runtime.
    Faulted,
    /// All owned work has been joined. This state is terminal.
    Stopped,
}

impl LifecycleState {
    pub(crate) fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    #[cfg(test)]
    pub(crate) fn is_stopped(self) -> bool {
        matches!(self, Self::Stopped)
    }
}

/// A cloneable, latched view of the process lifecycle.
///
/// Requesting either kind of stop changes the shared state for all current
/// and future observers. [`Self::wait_for_cancellation`] first reads the
/// current value before waiting, so a late observer cannot miss a request.
#[derive(Debug, Clone)]
pub struct Lifecycle {
    state: watch::Sender<LifecycleState>,
    faulted: Arc<AtomicBool>,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            state: watch::channel(LifecycleState::Running).0,
            faulted: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn state(&self) -> LifecycleState {
        *self.state.borrow()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.state().is_running()
    }

    pub fn is_cancelled(&self) -> bool {
        !self.is_running()
    }

    /// Whether a fault was ever latched. This remains true after the public
    /// state advances to terminal `Stopped`, preserving the final outcome.
    pub(crate) fn is_faulted(&self) -> bool {
        self.faulted.load(Ordering::SeqCst)
    }

    /// Request an intentional shutdown. Repeated and late graceful requests
    /// are idempotent and cannot override a fault or the terminal state.
    pub fn request_graceful_stop(&self) {
        self.transition(LifecycleState::GracefulStop);
    }

    /// Mark the runtime faulted. A fault always wins over a concurrent
    /// graceful request and is never downgraded by a later request.
    pub(crate) fn request_fault(&self) {
        self.faulted.store(true, Ordering::SeqCst);
        self.transition(LifecycleState::Faulted);
    }

    /// Enter the terminal state after all owned work has been joined.
    pub(crate) fn mark_stopped(&self) {
        self.transition(LifecycleState::Stopped);
    }

    /// Wait until cancellation is latched, returning the state that ended the
    /// wait. Reading before subscribing makes this race-free for late
    /// observers.
    pub async fn wait_for_cancellation(&self) -> LifecycleState {
        let mut state = self.state.subscribe();
        loop {
            let current = *state.borrow_and_update();
            if current != LifecycleState::Running {
                return current;
            }

            // A Lifecycle always retains its sender, so a changed error is
            // unreachable. Avoid spinning if the invariant is ever broken.
            if state.changed().await.is_err() {
                return LifecycleState::Faulted;
            }
        }
    }

    fn transition(&self, requested: LifecycleState) {
        self.state.send_if_modified(|current| {
            let next = match (*current, requested) {
                (LifecycleState::Running, LifecycleState::GracefulStop) => {
                    LifecycleState::GracefulStop
                }
                (
                    LifecycleState::Running | LifecycleState::GracefulStop,
                    LifecycleState::Faulted,
                ) => LifecycleState::Faulted,
                (state, LifecycleState::Stopped) => {
                    assert_ne!(
                        state,
                        LifecycleState::Running,
                        "Stopped requires a prior cancellation request"
                    );
                    LifecycleState::Stopped
                }
                (state, _) => state,
            };

            if next == *current {
                false
            } else {
                *current = next;
                true
            }
        });
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of joining one named child task.
#[derive(Debug)]
pub(crate) struct TaskCompletion<T = ()> {
    name: String,
    result: Result<T, JoinError>,
}

impl<T> TaskCompletion<T> {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    pub(crate) fn result(&self) -> &Result<T, JoinError> {
        &self.result
    }

    pub(crate) fn into_result(self) -> Result<T, JoinError> {
        self.result
    }

    pub(crate) fn is_success(&self) -> bool {
        self.result.is_ok()
    }
}

struct NamedTask<T> {
    name: String,
    handle: JoinHandle<T>,
}

fn validate_task_name(name: &str) {
    assert!(!name.trim().is_empty(), "task name must not be empty");
}

/// Explicit owner for a set of named child tasks.
///
/// A completion channel is deliberately not used as ownership: the group
/// retains every [`JoinHandle`], can join every child, and can abort and then
/// join every child. Child futures that need cooperative cancellation receive
/// [`Self::cancellation_token`] and apply cancellation at their own operation
/// boundaries.
pub(crate) struct TaskGroup<T = ()> {
    name: String,
    cancellation: CancellationToken,
    tasks: Vec<NamedTask<T>>,
}

impl<T: Send + 'static> TaskGroup<T> {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        assert!(!name.trim().is_empty(), "task group name must not be empty");
        Self {
            name,
            cancellation: CancellationToken::new(),
            tasks: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Spawn and register a named child.
    pub(crate) fn spawn<F>(&mut self, name: impl Into<String>, future: F)
    where
        F: Future<Output = T> + Send + 'static,
    {
        let name = name.into();
        validate_task_name(&name);
        self.adopt(name, tokio::spawn(future));
    }

    /// Adopt a pre-spawned handle. The returned handle remains owned by this
    /// group, so aborting the group's owner cannot detach the child.
    pub(crate) fn adopt(&mut self, name: impl Into<String>, handle: JoinHandle<T>) {
        let name = name.into();
        if name.trim().is_empty() {
            handle.abort();
            panic!("task name must not be empty");
        }
        if self.tasks.iter().any(|task| task.name == name) {
            handle.abort();
            panic!("duplicate task name {name:?} in group {:?}", self.name);
        }
        self.tasks.push(NamedTask { name, handle });
    }

    /// Request cooperative cancellation of children. Joining remains the
    /// parent's responsibility; this method does not detach or discard tasks.
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Join the next child to finish, reporting its stable name and full
    /// [`JoinError`] when it panicked or was cancelled.
    async fn join_next(&mut self) -> Option<TaskCompletion<T>> {
        if self.tasks.is_empty() {
            return None;
        }

        // Polling wrapper futures here is safe because they are not spawned:
        // cancelling this wait drops only the references. The original handles
        // remain in the group, avoiding the detached-inner-task trap.
        let completed = {
            let mut handles = FuturesUnordered::new();
            for (index, task) in self.tasks.iter_mut().enumerate() {
                handles.push(async move { (index, (&mut task.handle).await) });
            }
            handles.next().await
        };

        completed.map(|(index, result)| {
            let task = self.tasks.remove(index);
            TaskCompletion {
                name: task.name,
                result,
            }
        })
    }

    /// Join every child, including children added before this call, and
    /// return one completion record per child.
    pub(crate) async fn join_all(&mut self) -> Vec<TaskCompletion<T>> {
        let mut completions = Vec::with_capacity(self.tasks.len());
        while let Some(completion) = self.join_next().await {
            completions.push(completion);
        }
        completions
    }

    /// Request abortion of every unfinished child without waiting yet. This
    /// lets an owner stop all groups before it starts joining any of them.
    fn abort_all(&self) {
        for task in &self.tasks {
            task.handle.abort();
        }
    }

    /// Abort every unfinished child, then wait until every handle has been
    /// joined. The returned records include the abort-induced cancellation
    /// errors so callers cannot mistake this for a clean join.
    pub(crate) async fn abort_and_join(&mut self) -> Vec<TaskCompletion<T>> {
        self.abort_all();
        self.join_all().await
    }
}

impl<T> Drop for TaskGroup<T> {
    fn drop(&mut self) {
        // This is a last-resort guard against detaching children. Correct
        // owners still await abort_and_join so termination is confirmed.
        for task in &self.tasks {
            task.handle.abort();
        }
    }
}

/// The initial bounded cooperative-drain window for application-owned work.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// A failure reported by the top-level task supervisor.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SupervisorError {
    #[error("the external shutdown sender was dropped")]
    ShutdownSenderDropped,

    #[error("application task {task:?} exited unexpectedly")]
    TaskExited { task: String },

    #[error("application task {task:?} failed: {source}")]
    TaskFailed {
        task: String,
        #[source]
        source: JoinError,
    },

    #[error("task {task:?} in group {group:?} failed: {source}")]
    GroupTaskFailed {
        group: String,
        task: String,
        #[source]
        source: JoinError,
    },

    #[error("runtime lifecycle entered {state:?} without a task failure")]
    LifecycleFault { state: LifecycleState },

    #[error("task teardown exceeded {timeout:?}; unfinished work was aborted and joined")]
    DrainTimeout { timeout: Duration },
}

struct OwnedTaskCompletion {
    group: Option<String>,
    completion: TaskCompletion<()>,
}

enum ShutdownRequest {
    Graceful,
    Dropped,
}

enum StopReason {
    Shutdown(ShutdownRequest),
    Lifecycle(LifecycleState),
    DirectTask(TaskCompletion),
    GroupTask {
        group: String,
        completion: TaskCompletion,
    },
}

/// Top-level owner for all long-running application tasks and their explicit
/// child task groups.
///
/// Registration accepts both futures spawned by the supervisor and
/// pre-spawned [`JoinHandle`]s. The original handles are owned directly, so
/// the teardown fallback aborts and joins tasks rather than detaching inner
/// tasks behind wrapper futures.
pub(crate) struct TaskSupervisor {
    lifecycle: Lifecycle,
    tasks: TaskGroup<()>,
    groups: BTreeMap<String, TaskGroup<()>>,
    drain_timeout: Duration,
}

impl TaskSupervisor {
    pub(crate) fn new(lifecycle: Lifecycle) -> Self {
        Self {
            lifecycle,
            tasks: TaskGroup::new("application tasks"),
            groups: BTreeMap::new(),
            drain_timeout: DRAIN_TIMEOUT,
        }
    }

    /// Spawn and register a top-level application task.
    pub(crate) fn spawn<F>(&mut self, name: impl Into<String>, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.assert_accepting_tasks();
        self.tasks.spawn(name, future);
    }

    /// Adopt a pre-spawned top-level application task.
    pub(crate) fn adopt(&mut self, name: impl Into<String>, handle: JoinHandle<()>) {
        self.assert_accepting_tasks();
        self.tasks.adopt(name, handle);
    }

    /// Transfer ownership of an explicit child task group to the supervisor.
    #[cfg(test)]
    pub(crate) fn register_group(&mut self, group: TaskGroup<()>) {
        self.assert_accepting_tasks();
        let name = group.name().to_owned();
        let previous = self.groups.insert(name.clone(), group);
        assert!(previous.is_none(), "duplicate task group name {name:?}");
    }

    /// Run until external shutdown, a lifecycle fault, or any direct task
    /// completion. Then perform one bounded cooperative drain and join every
    /// remaining handle.
    pub(crate) async fn run(
        mut self,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), SupervisorError> {
        let stop = self.wait_for_stop(&mut shutdown).await;
        let mut error = match stop {
            StopReason::Shutdown(ShutdownRequest::Graceful) => {
                self.lifecycle.request_graceful_stop();
                None
            }
            StopReason::Shutdown(ShutdownRequest::Dropped) => {
                self.lifecycle.request_fault();
                Some(SupervisorError::ShutdownSenderDropped)
            }
            StopReason::Lifecycle(LifecycleState::GracefulStop) => None,
            StopReason::Lifecycle(state) => {
                if state != LifecycleState::Faulted {
                    self.lifecycle.request_fault();
                }
                Some(SupervisorError::LifecycleFault { state })
            }
            StopReason::DirectTask(completion) => {
                self.lifecycle.request_fault();
                Some(completion_error(None, completion))
            }
            StopReason::GroupTask { group, completion } => {
                self.lifecycle.request_fault();
                Some(completion_error(Some(&group), completion))
            }
        };

        self.request_group_cancellation();

        let drain = tokio::time::timeout(self.drain_timeout, self.join_owned_work()).await;
        match drain {
            Ok(completions) => {
                for owned in completions {
                    if !owned.completion.is_success() {
                        self.lifecycle.request_fault();
                        if error.is_none() {
                            error =
                                Some(completion_error(owned.group.as_deref(), owned.completion));
                        }
                    }
                }
                if error.is_none() && self.lifecycle.is_faulted() {
                    error = Some(SupervisorError::LifecycleFault {
                        state: LifecycleState::Faulted,
                    });
                }
            }
            Err(_) => {
                self.lifecycle.request_fault();
                if error.is_none() {
                    error = Some(SupervisorError::DrainTimeout {
                        timeout: self.drain_timeout,
                    });
                }
                self.abort_and_join_owned_work().await;
            }
        }

        // A timeout still enters Stopped only after abort_and_join confirms
        // that every application-owned handle has terminated.
        self.lifecycle.mark_stopped();
        let lifecycle_faulted = self.lifecycle.is_faulted();
        match error {
            Some(error) => Err(error),
            None if lifecycle_faulted => Err(SupervisorError::LifecycleFault {
                state: LifecycleState::Faulted,
            }),
            None if self.lifecycle.state() == LifecycleState::Stopped => Ok(()),
            None => Err(SupervisorError::LifecycleFault {
                state: self.lifecycle.state(),
            }),
        }
    }

    fn assert_accepting_tasks(&self) {
        assert!(
            self.lifecycle.is_running(),
            "cannot register task after lifecycle entered {:?}",
            self.lifecycle.state()
        );
    }

    async fn wait_for_stop(
        &mut self,
        shutdown: &mut tokio::sync::oneshot::Receiver<()>,
    ) -> StopReason {
        loop {
            let has_group_work = self.groups.values().any(|group| !group.is_empty());
            if !self.tasks.is_empty() || has_group_work {
                // Poll every direct and nested ownership source in one select.
                // Waiting on the first non-empty source would let a later task
                // or nested child fault without waking the supervisor.
                let mut completions = FuturesUnordered::<
                    Pin<
                        Box<
                            dyn Future<Output = (Option<String>, Option<TaskCompletion<()>>)>
                                + Send
                                + '_,
                        >,
                    >,
                >::new();
                if !self.tasks.is_empty() {
                    completions.push(Box::pin(async { (None, self.tasks.join_next().await) }));
                }
                for (group_name, group) in &mut self.groups {
                    if group.is_empty() {
                        continue;
                    }
                    let group_name = group_name.clone();
                    completions.push(Box::pin(async move {
                        (Some(group_name), group.join_next().await)
                    }));
                }

                let (group_name, completion) = tokio::select! {
                    biased;
                    completion = completions.select_next_some() => completion,
                    state = self.lifecycle.wait_for_cancellation() => {
                        return StopReason::Lifecycle(state);
                    }
                    request = &mut *shutdown => {
                        return StopReason::Shutdown(match request {
                            Ok(()) => ShutdownRequest::Graceful,
                            Err(_) => ShutdownRequest::Dropped,
                        });
                    }
                };

                let Some(completion) = completion else {
                    continue;
                };
                match group_name {
                    None => return StopReason::DirectTask(completion),
                    Some(group) if !completion.is_success() => {
                        return StopReason::GroupTask { group, completion };
                    }
                    Some(_) => {}
                }
                // A bounded child's clean completion is expected. Rebuild the
                // source set so the group's remaining children stay observed.
                continue;
            }

            return tokio::select! {
                biased;
                state = self.lifecycle.wait_for_cancellation() => {
                    StopReason::Lifecycle(state)
                }
                request = &mut *shutdown => {
                    StopReason::Shutdown(match request {
                        Ok(()) => ShutdownRequest::Graceful,
                        Err(_) => ShutdownRequest::Dropped,
                    })
                }
            };
        }
    }

    fn request_group_cancellation(&self) {
        self.tasks.cancel();
        for group in self.groups.values() {
            group.cancel();
        }
    }

    async fn join_owned_work(&mut self) -> Vec<OwnedTaskCompletion> {
        let mut completions = self
            .tasks
            .join_all()
            .await
            .into_iter()
            .map(|completion| OwnedTaskCompletion {
                group: None,
                completion,
            })
            .collect::<Vec<_>>();

        for (group_name, group) in &mut self.groups {
            completions.extend(group.join_all().await.into_iter().map(|completion| {
                OwnedTaskCompletion {
                    group: Some(group_name.clone()),
                    completion,
                }
            }));
        }
        completions
    }

    async fn abort_and_join_owned_work(&mut self) {
        let nested = self
            .groups
            .values_mut()
            .map(TaskGroup::abort_and_join)
            .collect::<Vec<_>>();
        let (direct, nested) = futures_util::future::join(
            self.tasks.abort_and_join(),
            futures_util::future::join_all(nested),
        )
        .await;
        drop(direct);
        drop(nested);
    }
}

fn completion_error(group: Option<&str>, completion: TaskCompletion<()>) -> SupervisorError {
    let task = completion.name().to_owned();
    match completion.into_result() {
        Ok(()) => SupervisorError::TaskExited { task },
        Err(source) => match group {
            Some(group) => SupervisorError::GroupTaskFailed {
                group: group.to_owned(),
                task,
                source,
            },
            None => SupervisorError::TaskFailed { task, source },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::{
        sync::oneshot,
        time::{Duration, Instant, timeout},
    };

    use super::{
        DRAIN_TIMEOUT, Lifecycle, LifecycleState, SupervisorError, TaskGroup, TaskSupervisor,
    };

    #[test]
    fn lifecycle_starts_running() {
        let lifecycle = Lifecycle::new();

        assert_eq!(lifecycle.state(), LifecycleState::Running);
        assert!(lifecycle.is_running());
        assert!(!lifecycle.is_cancelled());
    }

    #[tokio::test]
    async fn late_observer_sees_earlier_graceful_stop() {
        let lifecycle = Lifecycle::new();
        lifecycle.request_graceful_stop();

        let observed = timeout(Duration::from_millis(50), lifecycle.wait_for_cancellation())
            .await
            .expect("late observer should not miss a latched request");

        assert_eq!(observed, LifecycleState::GracefulStop);
    }

    #[tokio::test]
    async fn late_observer_sees_earlier_fault() {
        let lifecycle = Lifecycle::new();
        lifecycle.request_fault();

        let observed = timeout(Duration::from_millis(50), lifecycle.wait_for_cancellation())
            .await
            .expect("late observer should not miss a latched request");

        assert_eq!(observed, LifecycleState::Faulted);
    }

    #[test]
    fn repeated_stop_requests_are_idempotent() {
        let lifecycle = Lifecycle::new();

        lifecycle.request_graceful_stop();
        lifecycle.request_graceful_stop();
        lifecycle.request_graceful_stop();

        assert_eq!(lifecycle.state(), LifecycleState::GracefulStop);
    }

    #[tokio::test]
    async fn cancellation_remains_latched_after_the_first_request() {
        let lifecycle = Lifecycle::new();
        lifecycle.request_graceful_stop();

        for _ in 0..3 {
            let observed = timeout(Duration::from_millis(50), lifecycle.wait_for_cancellation())
                .await
                .expect("cancellation should stay latched");
            assert_eq!(observed, LifecycleState::GracefulStop);
        }
    }

    #[test]
    fn fault_wins_over_graceful_and_state_never_moves_back() {
        let lifecycle = Lifecycle::new();

        lifecycle.request_graceful_stop();
        lifecycle.request_fault();
        lifecycle.request_graceful_stop();

        assert_eq!(lifecycle.state(), LifecycleState::Faulted);
    }

    #[test]
    fn stopped_is_terminal() {
        let lifecycle = Lifecycle::new();
        lifecycle.request_fault();
        lifecycle.mark_stopped();

        lifecycle.request_fault();
        lifecycle.request_graceful_stop();

        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
        assert!(lifecycle.state().is_stopped());
        assert!(lifecycle.is_faulted());
    }

    #[tokio::test]
    async fn task_group_joins_every_child_with_stable_names() {
        let mut group = TaskGroup::new("bounded work");
        group.spawn("second", async {});
        group.spawn("first", async {});

        let mut completions = group.join_all().await;
        completions.sort_by(|left, right| left.name().cmp(right.name()));

        assert_eq!(completions.len(), 2);
        assert_eq!(completions[0].name(), "first");
        assert_eq!(completions[1].name(), "second");
        assert!(completions.iter().all(super::TaskCompletion::is_success));
        assert!(group.is_empty());
    }

    #[tokio::test]
    async fn task_group_preserves_domain_completion_values() {
        let mut group = TaskGroup::new("typed work");
        group.spawn("classified", async { 42 });

        let completions = group.join_all().await;

        assert_eq!(
            completions
                .into_iter()
                .next()
                .unwrap()
                .into_result()
                .unwrap(),
            42
        );
    }

    #[tokio::test]
    async fn task_group_reports_a_child_panic() {
        let mut group = TaskGroup::new("bounded work");
        group.spawn("panicking", async { panic!("child failed") });

        let completions = group.join_all().await;

        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].name(), "panicking");
        assert!(!completions[0].is_success());
        assert!(completions[0].result().as_ref().unwrap_err().is_panic());
    }

    #[tokio::test]
    async fn task_group_cancellation_is_cooperative_and_latched() {
        let mut group = TaskGroup::new("bounded work");
        let cancellation = group.cancellation_token();
        group.spawn("cooperative", async move {
            cancellation.cancelled().await;
        });

        group.cancel();
        group.cancel();
        let completions = group.join_all().await;

        assert_eq!(completions.len(), 1);
        assert!(completions[0].is_success());
    }

    #[tokio::test]
    async fn abort_and_join_confirms_termination_of_a_pre_spawned_child() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let mut group = TaskGroup::new("bounded work");
        group.adopt(
            "pre-spawned",
            tokio::spawn(async move {
                let _flag = flag;
                std::future::pending::<()>().await;
            }),
        );
        tokio::task::yield_now().await;

        let completions = group.abort_and_join().await;

        assert_eq!(completions.len(), 1);
        assert!(!completions[0].is_success());
        assert!(completions[0].result().as_ref().unwrap_err().is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(group.is_empty());
    }

    #[tokio::test]
    async fn empty_supervisor_drains_cleanly_after_external_stop() {
        let lifecycle = Lifecycle::new();
        let supervisor = TaskSupervisor::new(lifecycle.clone());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        supervisor.run(shutdown_rx).await.unwrap();

        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
    }

    #[tokio::test]
    async fn direct_task_return_before_stop_is_a_fault() {
        let mut supervisor = TaskSupervisor::new(Lifecycle::new());
        supervisor.spawn("early-return", async {});
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::TaskExited { task } if task == "early-return"
        ));
    }

    #[tokio::test]
    async fn direct_task_panic_before_stop_is_a_fault() {
        let mut supervisor = TaskSupervisor::new(Lifecycle::new());
        supervisor.spawn("panic", async { panic!("direct task failed") });
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::TaskFailed { task, .. } if task == "panic"
        ));
    }

    #[tokio::test]
    async fn task_panic_during_graceful_drain_remains_a_fault() {
        let lifecycle = Lifecycle::new();
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.spawn("panic-on-stop", async move {
            lifecycle.wait_for_cancellation().await;
            panic!("failed during drain");
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::TaskFailed { task, .. } if task == "panic-on-stop"
        ));
    }

    #[tokio::test]
    async fn lifecycle_fault_during_graceful_drain_remains_a_fault() {
        let lifecycle = Lifecycle::new();
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.spawn("fault-on-stop", {
            let lifecycle = lifecycle.clone();
            async move {
                lifecycle.wait_for_cancellation().await;
                lifecycle.request_fault();
            }
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::LifecycleFault {
                state: LifecycleState::Faulted
            }
        ));
    }

    #[tokio::test]
    async fn dropped_external_shutdown_sender_is_a_fault() {
        let supervisor = TaskSupervisor::new(Lifecycle::new());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        drop(shutdown_tx);

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(error, SupervisorError::ShutdownSenderDropped));
    }

    #[tokio::test]
    async fn ready_task_fault_wins_over_ready_external_stop() {
        let mut supervisor = TaskSupervisor::new(Lifecycle::new());
        let (finish_tx, finish_rx) = oneshot::channel();
        supervisor.spawn("race", async move {
            finish_rx.await.unwrap();
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::task::yield_now().await;

        finish_tx.send(()).unwrap();
        tokio::task::yield_now().await;
        shutdown_tx.send(()).unwrap();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::TaskExited { task } if task == "race"
        ));
    }

    #[tokio::test]
    async fn latched_fault_wins_over_external_stop() {
        let lifecycle = Lifecycle::new();
        lifecycle.request_fault();
        let supervisor = TaskSupervisor::new(lifecycle);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::LifecycleFault {
                state: LifecycleState::Faulted
            }
        ));
    }

    #[tokio::test]
    async fn registered_group_is_cancelled_and_its_descendants_are_joined() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let joined = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&joined));
        let mut group = TaskGroup::new("nested");
        let cancellation = group.cancellation_token();
        group.spawn("descendant", async move {
            let _flag = flag;
            cancellation.cancelled().await;
        });

        let lifecycle = Lifecycle::new();
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.register_group(group);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        supervisor.run(shutdown_rx).await.unwrap();

        assert!(joined.load(Ordering::SeqCst));
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
    }

    #[tokio::test]
    async fn registered_group_panic_faults_the_supervisor() {
        let mut group = TaskGroup::new("nested");
        group.spawn("group-panic", async { panic!("nested child failed") });
        let mut supervisor = TaskSupervisor::new(Lifecycle::new());
        supervisor.register_group(group);
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::GroupTaskFailed {
                group,
                task,
                ..
            } if group == "nested" && task == "group-panic"
        ));
    }

    #[tokio::test]
    async fn registered_group_panic_wakes_while_a_direct_task_is_still_running() {
        let mut group = TaskGroup::new("nested");
        group.spawn("group-panic", async { panic!("nested child failed") });
        let lifecycle = Lifecycle::new();
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.spawn("still-running", async move {
            lifecycle.wait_for_cancellation().await;
        });
        supervisor.register_group(group);
        tokio::task::yield_now().await;
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::GroupTaskFailed {
                group,
                task,
                ..
            } if group == "nested" && task == "group-panic"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn exact_drain_timeout_aborts_and_joins_a_pre_spawned_handle() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let pre_spawned = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        let lifecycle = Lifecycle::new();
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.adopt("stubborn", pre_spawned);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();
        let started = Instant::now();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert_eq!(started.elapsed(), DRAIN_TIMEOUT);
        assert!(matches!(
            error,
            SupervisorError::DrainTimeout { timeout } if timeout == DRAIN_TIMEOUT
        ));
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
    }
}
