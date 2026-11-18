//! Latched application lifecycle state.
//!
//! This module deliberately contains no domain policy. It only gives
//! application-owned tasks one cheap, cloneable way to observe and request
//! runtime shutdown without missing an earlier request.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use tokio::{
    sync::{Mutex, watch},
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
    cancellation: CancellationToken,
    faulted: Arc<AtomicBool>,
    child_tasks: ChildTaskRegistry,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            state: watch::channel(LifecycleState::Running).0,
            cancellation: CancellationToken::new(),
            faulted: Arc::new(AtomicBool::new(false)),
            child_tasks: ChildTaskRegistry::default(),
        }
    }

    pub fn state(&self) -> LifecycleState {
        *self.state.borrow()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.state().is_running()
    }

    pub fn is_cancelled(&self) -> bool {
        !self.is_running()
    }

    /// Clone the lifecycle's single latched cancellation token.
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Whether a fault was ever latched. This remains true after the public
    /// state advances to terminal `Stopped`, preserving the final outcome.
    pub fn is_faulted(&self) -> bool {
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
        let transitioned = self.state.send_if_modified(|current| {
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

        // Admission checks must observe the non-running state before any
        // token-only observer is released. This ordering is part of the
        // lifecycle contract shared by command ingress and execution.
        if transitioned {
            self.cancellation.cancel();
        }
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

    #[cfg(test)]
    pub(crate) fn is_success(&self) -> bool {
        self.result.is_ok()
    }
}

fn validate_task_name(name: &str) {
    assert!(!name.trim().is_empty(), "task name must not be empty");
}

/// Global ownership for application-created descendants.
///
/// Child handles live here, rather than only in the parent task's local data.
/// That lets the top-level supervisor cancel, abort, and join descendants even
/// when the parent that created them is forcibly aborted first.
#[derive(Debug, Clone, Default)]
struct ChildTaskRegistry {
    inner: Arc<Mutex<ChildTasksInner>>,
}

impl TaskCompletion<eyre::Result<()>> {
    fn is_clean(&self) -> bool {
        matches!(&self.result, Ok(Ok(())))
    }
}

#[derive(Debug, Default)]
struct ChildTasksInner {
    tasks: Vec<Option<RegisteredChildTask>>,
    free_ids: Vec<usize>,
}

#[derive(Debug)]
struct RegisteredChildTask {
    group: String,
    task: String,
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

struct GlobalTaskCompletion {
    id: usize,
    group: String,
    task: String,
    result: Result<(), JoinError>,
}

impl ChildTaskRegistry {
    async fn request_cancellation(&self) {
        for task in self.inner.lock().await.tasks.iter().flatten() {
            task.cancellation.cancel();
        }
    }

    async fn join_next_for(&self, ids: &BTreeSet<usize>) -> Option<GlobalTaskCompletion> {
        if ids.is_empty() {
            return None;
        }

        let mut inner = self.inner.lock().await;
        let mut handles = FuturesUnordered::new();
        for (id, task) in inner.tasks.iter_mut().enumerate() {
            if !ids.contains(&id) {
                continue;
            }
            let task = task
                .as_mut()
                .expect("task group and child registry ownership diverged");
            handles.push(async move { (id, (&mut task.handle).await) });
        }
        let (id, result) = handles.next().await?;
        drop(handles);
        let task = inner.tasks[id]
            .take()
            .expect("completed child task disappeared from registry");
        inner.free_ids.push(id);
        Some(GlobalTaskCompletion {
            id,
            group: task.group,
            task: task.task,
            result,
        })
    }

    async fn join_next(&self) -> Option<GlobalTaskCompletion> {
        let inner = self.inner.lock().await;
        let ids = inner
            .tasks
            .iter()
            .enumerate()
            .filter_map(|(id, task)| task.as_ref().map(|_| id))
            .collect::<BTreeSet<_>>();
        drop(inner);
        self.join_next_for(&ids).await
    }

    async fn join_all(&self) -> Vec<GlobalTaskCompletion> {
        let mut completions = Vec::new();
        while let Some(completion) = self.join_next().await {
            completions.push(completion);
        }
        completions
    }

    async fn abort_all(&self) {
        for task in self.inner.lock().await.tasks.iter().flatten() {
            task.handle.abort();
        }
    }

    #[expect(
        dead_code,
        reason = "TaskGroup::abort_and_join is part of the explicit child-owner contract"
    )]
    async fn abort_group(&self, ids: &BTreeSet<usize>) {
        let inner = self.inner.lock().await;
        for id in ids {
            inner.tasks[*id]
                .as_ref()
                .expect("task group and child registry ownership diverged")
                .handle
                .abort();
        }
    }
}

impl Drop for ChildTaskRegistry {
    fn drop(&mut self) {
        // Correct supervision removes completed children. Aborting the actual
        // registered handles here is only a last-resort guard if composition
        // unwinds before the supervisor takes over. Only the final registry
        // owner may do this; dropping one task group's clone must not abort
        // unrelated groups.
        if Arc::strong_count(&self.inner) == 1
            && let Ok(inner) = self.inner.try_lock()
        {
            for task in inner.tasks.iter().flatten() {
                task.handle.abort();
            }
        }
    }
}

struct RegisteredTask<T> {
    name: String,
    output: Arc<Mutex<Option<T>>>,
}

/// Explicit owner for a set of named nested child tasks.
///
/// The parent keeps typed output slots and a stable group name for normal
/// reconciliation. The lifecycle's global registry owns the actual task
/// handles, so dropping or aborting the parent cannot detach descendants.
pub(crate) struct TaskGroup<T = ()> {
    name: String,
    cancellation: CancellationToken,
    registry: ChildTaskRegistry,
    tasks: BTreeMap<usize, RegisteredTask<T>>,
}

impl<T: Send + 'static> TaskGroup<T> {
    pub(crate) fn new(lifecycle: &Lifecycle, name: impl Into<String>) -> Self {
        let name = name.into();
        assert!(!name.trim().is_empty(), "task group name must not be empty");
        Self {
            name,
            cancellation: CancellationToken::new(),
            registry: lifecycle.child_tasks.clone(),
            tasks: BTreeMap::new(),
        }
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Spawn and register a named child with both its parent group and the
    /// global child registry. The spawned future is the registered task itself;
    /// no inner pre-spawned handle is hidden behind a detachable wrapper.
    pub(crate) async fn spawn<F>(&mut self, name: impl Into<String>, future: F)
    where
        F: Future<Output = T> + Send + 'static,
    {
        let name = name.into();
        validate_task_name(&name);
        if self.tasks.values().any(|task| task.name == name) {
            panic!("duplicate task name {name:?} in group {:?}", self.name);
        }

        // Acquire the registry lock before spawning. After the handle exists,
        // registration is synchronous: parent cancellation can never drop a
        // completed or still-running inner task between spawn and adoption.
        let mut registry = self.registry.inner.lock().await;
        let output = Arc::new(Mutex::new(None));
        let task_output = Arc::clone(&output);
        let handle = tokio::spawn(async move {
            *task_output.lock().await = Some(future.await);
        });
        let id = registry.free_ids.pop().unwrap_or(registry.tasks.len());
        let task = RegisteredChildTask {
            group: self.name.clone(),
            task: name.clone(),
            cancellation: self.cancellation.clone(),
            handle,
        };
        if id == registry.tasks.len() {
            registry.tasks.push(Some(task));
        } else {
            registry.tasks[id] = Some(task);
        }
        drop(registry);
        self.tasks.insert(id, RegisteredTask { name, output });
    }

    /// Request cooperative cancellation of children. Joining remains the
    /// parent's responsibility; this method does not detach or discard tasks.
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    async fn join_next(&mut self) -> Option<TaskCompletion<T>> {
        if self.tasks.is_empty() {
            return None;
        }
        let ids = self.tasks.keys().copied().collect::<BTreeSet<_>>();
        let completed = self.registry.join_next_for(&ids).await?;
        let task = self
            .tasks
            .remove(&completed.id)
            .expect("completed task disappeared from parent group");
        let output = task.output.lock().await.take();
        let result = match completed.result {
            Ok(()) => Ok(output.expect("successful child task must publish its output")),
            Err(error) => Err(error),
        };
        Some(TaskCompletion {
            name: completed.task,
            result,
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

    /// Abort every unfinished child, then wait until every handle has been
    /// joined. The returned records include the abort-induced cancellation
    /// errors so callers cannot mistake this for a clean join.
    #[expect(
        dead_code,
        reason = "explicit abort-and-join is part of the TaskGroup ownership contract"
    )]
    pub(crate) async fn abort_and_join(&mut self) -> Vec<TaskCompletion<T>> {
        self.cancel();
        let ids = self.tasks.keys().copied().collect::<BTreeSet<_>>();
        self.registry.abort_group(&ids).await;
        self.join_all().await
    }
}

impl<T> Drop for TaskGroup<T> {
    fn drop(&mut self) {
        // Handles stay globally registered and are cancelled rather than
        // removed. The supervisor can still abort and join them after this
        // parent future is dropped.
        self.cancellation.cancel();
    }
}

type DirectTaskResult = Result<eyre::Result<()>, JoinError>;
type DirectJoinFuture<'a> = Pin<Box<dyn Future<Output = (usize, DirectTaskResult)> + Send + 'a>>;

/// Direct owner for top-level tasks. Nested application work uses [`TaskGroup`],
/// whose handles are also retained in the global registry.
pub(crate) struct LocalTaskGroup {
    name: String,
    cancellation: CancellationToken,
    tasks: Vec<NamedDirectTask>,
}

impl LocalTaskGroup {
    fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        assert!(!name.trim().is_empty(), "task group name must not be empty");
        Self {
            name,
            cancellation: CancellationToken::new(),
            tasks: Vec::new(),
        }
    }

    fn spawn<F>(&mut self, name: impl Into<String>, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.adopt(name, tokio::spawn(future));
    }

    fn spawn_result<F, E>(&mut self, name: impl Into<String>, future: F)
    where
        F: Future<Output = Result<(), E>> + Send + 'static,
        E: Into<eyre::Report> + Send + 'static,
    {
        self.adopt_result(
            name,
            tokio::spawn(async move { future.await.map_err(Into::into) }),
        );
    }

    fn adopt(&mut self, name: impl Into<String>, handle: JoinHandle<()>) {
        self.push(name, DirectTask::Unit(handle));
    }

    fn adopt_result(&mut self, name: impl Into<String>, handle: JoinHandle<eyre::Result<()>>) {
        self.push(name, DirectTask::Result(handle));
    }

    /// Register an already spawned task. A rejected name panics, which drops
    /// `task` and thereby aborts its handle.
    fn push(&mut self, name: impl Into<String>, task: DirectTask) {
        let name = name.into();
        if name.trim().is_empty() {
            panic!("task name must not be empty");
        }
        if self.tasks.iter().any(|task| task.name == name) {
            panic!("duplicate task name {name:?} in group {:?}", self.name);
        }
        self.tasks.push(NamedDirectTask { name, task });
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }

    async fn join_next(&mut self) -> Option<TaskCompletion<eyre::Result<()>>> {
        if self.tasks.is_empty() {
            return None;
        }
        let completed = {
            let mut handles: FuturesUnordered<DirectJoinFuture<'_>> = FuturesUnordered::new();
            for (index, task) in self.tasks.iter_mut().enumerate() {
                match &mut task.task {
                    DirectTask::Unit(handle) => {
                        handles.push(Box::pin(
                            async move { (index, handle.await.map(|_| Ok(()))) },
                        ));
                    }
                    DirectTask::Result(handle) => {
                        handles.push(Box::pin(async move { (index, handle.await) }));
                    }
                }
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

    async fn join_all(&mut self) -> Vec<TaskCompletion<eyre::Result<()>>> {
        let mut completions = Vec::with_capacity(self.tasks.len());
        while let Some(completion) = self.join_next().await {
            completions.push(completion);
        }
        completions
    }

    fn abort_all(&self) {
        for task in &self.tasks {
            task.task.abort();
        }
    }

    async fn abort_and_join(&mut self) -> Vec<TaskCompletion<eyre::Result<()>>> {
        self.abort_all();
        self.join_all().await
    }
}

enum DirectTask {
    Unit(JoinHandle<()>),
    Result(JoinHandle<eyre::Result<()>>),
}

impl DirectTask {
    fn abort(&self) {
        match self {
            Self::Unit(handle) => handle.abort(),
            Self::Result(handle) => handle.abort(),
        }
    }
}

impl Drop for DirectTask {
    fn drop(&mut self) {
        self.abort();
    }
}

struct NamedDirectTask {
    name: String,
    task: DirectTask,
}

impl Drop for LocalTaskGroup {
    fn drop(&mut self) {
        self.abort_all();
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

    #[error("application task {task:?} returned an error")]
    TaskReturnedError {
        task: String,
        #[source]
        source: eyre::Report,
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

    #[error("lifecycle finalizer {finalizer:?} failed")]
    FinalizerFailed {
        finalizer: String,
        #[source]
        source: eyre::Report,
    },
}

enum OwnedTaskCompletion {
    Direct(TaskCompletion<eyre::Result<()>>),
    Child(TaskCompletion<()>),
}

struct OwnedTask {
    group: Option<String>,
    completion: OwnedTaskCompletion,
}

struct NamedFinalizer {
    name: String,
    future: Pin<Box<dyn Future<Output = eyre::Result<()>> + Send + 'static>>,
}

enum ShutdownRequest {
    Graceful,
    Dropped,
}

enum StopReason {
    Shutdown(ShutdownRequest),
    Lifecycle(LifecycleState),
    DirectTask(TaskCompletion<eyre::Result<()>>),
}

/// Top-level owner for all long-running application tasks, globally registered
/// descendants, and the finalizer phase.
pub(crate) struct TaskSupervisor {
    lifecycle: Lifecycle,
    tasks: LocalTaskGroup,
    finalizers: Vec<NamedFinalizer>,
    drain_timeout: Duration,
}

impl TaskSupervisor {
    pub(crate) fn new(lifecycle: Lifecycle) -> Self {
        Self {
            lifecycle,
            tasks: LocalTaskGroup::new("application tasks"),
            finalizers: Vec::new(),
            drain_timeout: DRAIN_TIMEOUT,
        }
    }

    /// Spawn and register a top-level application task.
    pub(crate) fn spawn<F>(&mut self, name: impl Into<String>, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.assert_accepting_work();
        self.tasks.spawn(name, future);
    }

    /// Spawn and register a result-bearing top-level application task.
    /// `Err` is retained as a task failure during both initial supervision and
    /// the graceful drain; it is never flattened into an ordinary clean exit.
    pub(crate) fn spawn_result<F, E>(&mut self, name: impl Into<String>, future: F)
    where
        F: Future<Output = Result<(), E>> + Send + 'static,
        E: Into<eyre::Report> + Send + 'static,
    {
        self.assert_accepting_work();
        self.tasks.spawn_result(name, future);
    }

    /// Adopt a pre-spawned top-level application task without wrapping its
    /// handle in a detachable second task.
    pub(crate) fn adopt(&mut self, name: impl Into<String>, handle: JoinHandle<()>) {
        self.assert_accepting_work();
        self.tasks.adopt(name, handle);
    }

    /// Adopt a pre-spawned, result-bearing top-level application task. `Err`
    /// is handled exactly like a [`spawn_result`](Self::spawn_result) error.
    pub(crate) fn adopt_result(
        &mut self,
        name: impl Into<String>,
        handle: JoinHandle<eyre::Result<()>>,
    ) {
        self.assert_accepting_work();
        self.tasks.adopt_result(name, handle);
    }

    /// Register ordered, application-owned cleanup to run only after all task
    /// producers and globally registered descendants have been joined.
    pub(crate) fn register_finalizer<F>(&mut self, name: impl Into<String>, future: F)
    where
        F: Future<Output = eyre::Result<()>> + Send + 'static,
    {
        self.assert_accepting_work();
        let name = name.into();
        validate_task_name(&name);
        assert!(
            self.finalizers
                .iter()
                .all(|finalizer| finalizer.name != name),
            "duplicate finalizer name {name:?}"
        );
        self.finalizers.push(NamedFinalizer {
            name,
            future: Box::pin(future),
        });
    }

    /// Run until external shutdown, a lifecycle fault, or any direct task
    /// completion. Then drain all owned work, run finalizers, and only then
    /// enter terminal `Stopped`.
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
                Some(direct_stop_error(completion))
            }
        };

        let drain = tokio::time::timeout(self.drain_timeout, async {
            // Cancellation registration is part of the same five-second
            // cooperative window as joining. Slow group registration must not
            // extend that window before work is even asked to stop.
            self.request_child_cancellation().await;
            self.join_owned_work().await
        })
        .await;
        match drain {
            Ok(completions) => {
                for owned in completions {
                    let failure = match owned.completion {
                        OwnedTaskCompletion::Direct(completion) => {
                            direct_completion_error(completion)
                        }
                        OwnedTaskCompletion::Child(completion) => {
                            child_completion_error(owned.group.as_deref(), completion)
                        }
                    };
                    if let Some(failure) = failure {
                        self.lifecycle.request_fault();
                        if error.is_none() {
                            error = Some(failure);
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

        // Every finalizer failure Faults the runtime. The first becomes the
        // returned error when nothing failed earlier; any other would be
        // dropped, so it is logged here.
        for (finalizer, source) in self.run_finalizers().await {
            self.lifecycle.request_fault();
            if error.is_none() {
                error = Some(SupervisorError::FinalizerFailed { finalizer, source });
            } else {
                tracing::error!(finalizer, error = ?source, "Lifecycle finalizer failed");
            }
        }

        // A timeout or finalizer fault still enters Stopped only after every
        // owned task and finalizer has completed or been joined.
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

    fn assert_accepting_work(&self) {
        assert!(
            self.lifecycle.is_running(),
            "cannot register task or finalizer after lifecycle entered {:?}",
            self.lifecycle.state()
        );
    }

    async fn wait_for_stop(
        &mut self,
        shutdown: &mut tokio::sync::oneshot::Receiver<()>,
    ) -> StopReason {
        if self.tasks.is_empty() {
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

        tokio::select! {
            biased;
            completion = self.tasks.join_next() => {
                let completion = completion.expect("non-empty task group must complete");
                if completion.is_clean() && self.lifecycle.is_cancelled() {
                    // Lifecycle state is latched before its cancellation token,
                    // so a token-driven task return is expected even when the
                    // supervisor observes both futures ready together.
                    StopReason::Lifecycle(self.lifecycle.state())
                } else {
                    StopReason::DirectTask(completion)
                }
            }
            state = self.lifecycle.wait_for_cancellation() => {
                StopReason::Lifecycle(state)
            }
            request = &mut *shutdown => {
                StopReason::Shutdown(match request {
                    Ok(()) => ShutdownRequest::Graceful,
                    Err(_) => ShutdownRequest::Dropped,
                })
            }
        }
    }

    async fn request_child_cancellation(&mut self) {
        self.tasks.cancel();
        self.lifecycle.child_tasks.request_cancellation().await;
    }

    async fn join_owned_work(&mut self) -> Vec<OwnedTask> {
        let mut completions = self
            .tasks
            .join_all()
            .await
            .into_iter()
            .map(|completion| OwnedTask {
                group: None,
                completion: OwnedTaskCompletion::Direct(completion),
            })
            .collect::<Vec<_>>();

        completions.extend(self.lifecycle.child_tasks.join_all().await.into_iter().map(
            |completion| OwnedTask {
                group: Some(completion.group),
                completion: OwnedTaskCompletion::Child(TaskCompletion {
                    name: completion.task,
                    result: completion.result,
                }),
            },
        ));
        completions
    }

    async fn abort_and_join_owned_work(&mut self) {
        // Abort and join direct tasks first. Dropping each aborted parent
        // releases any registry lock held while that parent joined a child.
        // The child handles then remain owned by the global registry and are
        // explicitly aborted and joined here as a complete descendant graph.
        let _ = self.tasks.abort_and_join().await;
        self.lifecycle.child_tasks.abort_all().await;
        let _ = self.lifecycle.child_tasks.join_all().await;
    }

    async fn run_finalizers(&mut self) -> Vec<(String, eyre::Report)> {
        let mut failures = Vec::new();
        for finalizer in std::mem::take(&mut self.finalizers) {
            match AssertUnwindSafe(finalizer.future).catch_unwind().await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push((finalizer.name, error)),
                Err(_) => failures.push((finalizer.name, eyre::eyre!("finalizer panicked"))),
            }
        }
        failures
    }
}

fn direct_stop_error(completion: TaskCompletion<eyre::Result<()>>) -> SupervisorError {
    let task = completion.name().to_owned();
    match completion.into_result() {
        Ok(Ok(())) => SupervisorError::TaskExited { task },
        Ok(Err(source)) => SupervisorError::TaskReturnedError { task, source },
        Err(source) => SupervisorError::TaskFailed { task, source },
    }
}

fn direct_completion_error(
    completion: TaskCompletion<eyre::Result<()>>,
) -> Option<SupervisorError> {
    let task = completion.name().to_owned();
    match completion.into_result() {
        Ok(Ok(())) => None,
        Ok(Err(source)) => Some(SupervisorError::TaskReturnedError { task, source }),
        Err(source) => Some(SupervisorError::TaskFailed { task, source }),
    }
}

fn child_completion_error(
    group: Option<&str>,
    completion: TaskCompletion<()>,
) -> Option<SupervisorError> {
    let task = completion.name().to_owned();
    match completion.into_result() {
        Ok(()) => None,
        Err(source) => Some(match group {
            Some(group) => SupervisorError::GroupTaskFailed {
                group: group.to_owned(),
                task,
                source,
            },
            None => SupervisorError::TaskFailed { task, source },
        }),
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
        DRAIN_TIMEOUT, Lifecycle, LifecycleState, LocalTaskGroup, SupervisorError, TaskGroup,
        TaskSupervisor,
    };

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
    async fn lifecycle_token_is_cancelled_only_after_state_leaves_running() {
        let lifecycle = Lifecycle::new();
        let cancellation = lifecycle.cancellation_token();
        assert!(!cancellation.is_cancelled());
        assert_eq!(lifecycle.state(), LifecycleState::Running);

        lifecycle.request_graceful_stop();

        assert_eq!(lifecycle.state(), LifecycleState::GracefulStop);
        assert!(cancellation.is_cancelled());
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
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "bounded work");
        group.spawn("second", async {}).await;
        group.spawn("first", async {}).await;

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
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "typed work");
        group.spawn("classified", async { 42 }).await;

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
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "bounded work");
        group
            .spawn("panicking", async { panic!("child failed") })
            .await;

        let completions = group.join_all().await;

        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].name(), "panicking");
        assert!(!completions[0].is_success());
        assert!(completions[0].result().as_ref().unwrap_err().is_panic());
    }

    #[tokio::test]
    async fn task_group_cancellation_is_cooperative_and_latched() {
        let lifecycle = Lifecycle::new();
        let mut group = TaskGroup::new(&lifecycle, "bounded work");
        let cancellation = group.cancellation_token();
        group
            .spawn("cooperative", async move {
                cancellation.cancelled().await;
            })
            .await;

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
        let mut group = LocalTaskGroup::new("bounded work");
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
    async fn token_driven_clean_return_after_lifecycle_stop_is_expected() {
        let lifecycle = Lifecycle::new();
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.spawn_result("token-driven", {
            let lifecycle = lifecycle.clone();
            async move {
                lifecycle.wait_for_cancellation().await;
                Ok::<_, std::convert::Infallible>(())
            }
        });
        lifecycle.request_graceful_stop();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();

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

    /// Messages of every error below `error` in its `source()` chain.
    fn source_messages(error: &dyn std::error::Error) -> Vec<String> {
        std::iter::successors(error.source(), |source| source.source())
            .map(ToString::to_string)
            .collect()
    }

    #[tokio::test]
    async fn result_task_error_keeps_its_cause_chain() {
        let mut supervisor = TaskSupervisor::new(Lifecycle::new());
        supervisor.spawn_result("domain-failed", async {
            Err(eyre::eyre!("inner").wrap_err("outer"))
        });
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::TaskReturnedError { ref task, .. } if task == "domain-failed"
        ));
        let chain = source_messages(&error);
        assert!(chain.iter().any(|message| message == "outer"), "{chain:?}");
        assert!(chain.iter().any(|message| message == "inner"), "{chain:?}");
    }

    #[tokio::test]
    async fn nested_child_failure_is_reported_by_its_direct_owner() {
        let lifecycle = Lifecycle::new();
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.spawn_result("child-owner", {
            let lifecycle = lifecycle.clone();
            async move {
                let mut children = TaskGroup::<()>::new(&lifecycle, "owned child");
                children
                    .spawn("panicking child", async { panic!("nested failure") })
                    .await;
                let completions = children.join_all().await;
                let child_panicked = completions.into_iter().any(|completion| {
                    completion
                        .into_result()
                        .is_err_and(|source| source.is_panic())
                });
                if child_panicked {
                    Err(eyre::eyre!("owner observed nested child panic"))
                } else {
                    Ok(())
                }
            }
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::TaskReturnedError { ref task, ref source }
                if task == "child-owner"
                    && source.to_string() == "owner observed nested child panic"
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

    #[tokio::test(start_paused = true)]
    async fn parent_abort_joins_globally_registered_descendants() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let descendant_dropped = Arc::new(AtomicBool::new(false));
        let lifecycle = Lifecycle::new();
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.spawn("parent", {
            let lifecycle = lifecycle.clone();
            let descendant_dropped = Arc::clone(&descendant_dropped);
            async move {
                let mut children = TaskGroup::new(&lifecycle, "nested");
                let flag = DropFlag(descendant_dropped);
                children
                    .spawn("descendant", async move {
                        let _flag = flag;
                        std::future::pending::<()>().await;
                    })
                    .await;
                std::future::pending::<()>().await;
            }
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(error, SupervisorError::DrainTimeout { .. }));
        assert!(descendant_dropped.load(Ordering::SeqCst));
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
    }

    #[tokio::test]
    async fn finalizers_run_after_drain_and_before_stopped() {
        let lifecycle = Lifecycle::new();
        let task_joined = Arc::new(AtomicBool::new(false));
        let observed_state = Arc::new(std::sync::Mutex::new(None));
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.spawn("producer", {
            let lifecycle = lifecycle.clone();
            let task_joined = Arc::clone(&task_joined);
            async move {
                lifecycle.wait_for_cancellation().await;
                task_joined.store(true, Ordering::SeqCst);
            }
        });
        supervisor.register_finalizer("test finalizer", {
            let lifecycle = lifecycle.clone();
            let task_joined = Arc::clone(&task_joined);
            let observed_state = Arc::clone(&observed_state);
            async move {
                assert!(task_joined.load(Ordering::SeqCst));
                *observed_state.lock().unwrap() = Some(lifecycle.state());
                assert_eq!(lifecycle.state(), LifecycleState::GracefulStop);
                Ok(())
            }
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        supervisor.run(shutdown_rx).await.unwrap();

        assert_eq!(
            *observed_state.lock().unwrap(),
            Some(LifecycleState::GracefulStop)
        );
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
    }

    #[tokio::test]
    async fn finalizer_failure_faults_before_stopped() {
        let lifecycle = Lifecycle::new();
        let observed_stopped = Arc::new(AtomicBool::new(false));
        let mut supervisor = TaskSupervisor::new(lifecycle.clone());
        supervisor.register_finalizer("failing finalizer", {
            let lifecycle = lifecycle.clone();
            let observed_stopped = Arc::clone(&observed_stopped);
            async move {
                observed_stopped.store(
                    lifecycle.state() == LifecycleState::Stopped,
                    Ordering::SeqCst,
                );
                Err(eyre::eyre!("cleanup failed"))
            }
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();

        let error = supervisor.run(shutdown_rx).await.unwrap_err();

        assert!(matches!(
            error,
            SupervisorError::FinalizerFailed { ref finalizer, ref source }
                if finalizer == "failing finalizer" && source.to_string() == "cleanup failed"
        ));
        assert!(!observed_stopped.load(Ordering::SeqCst));
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
        assert!(lifecycle.is_faulted());
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
