use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use eyre::Result;
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use twitch_irc::{login::LoginCredentials, message::PrivmsgMessage, transport::Transport};

use super::{Command, CommandId, CommandRegistry, ExecutionClass, normalize_username};
use crate::twitch::ChatSender;

/// Capacity of the task-to-task admission handoff.
pub const COMMAND_ADMISSION_CAPACITY: usize = 256;
/// Maximum number of accepted background commands executing at once.
pub const MAX_ACTIVE_BACKGROUND_COMMANDS: usize = 32;
/// Maximum number of accepted background commands waiting for capacity.
pub const MAX_PENDING_BACKGROUND_COMMANDS: usize = 128;
/// Maximum active command count for one stable user key.
pub const MAX_ACTIVE_COMMANDS_PER_USER: usize = 3;
/// Maximum queued command count for one stable user key.
pub const MAX_PENDING_COMMANDS_PER_USER: usize = 8;
/// Maximum active command count for one canonical command ID.
pub const MAX_ACTIVE_COMMANDS_PER_COMMAND: usize = 8;
/// Maximum queued command count for one canonical command ID.
pub const MAX_PENDING_COMMANDS_PER_COMMAND: usize = 8;

/// The only generic command response emitted by the execution layer.
pub const CAPACITY_REJECTION_MESSAGE: &str = "zu viel zu tun";

/// Cancellation capability checked at command operation boundaries.
///
/// The manager receives this from Candidate 08. It deliberately carries no
/// process-wide lifecycle state and has no deadline of its own.
#[derive(Clone, Debug, Default)]
pub struct CommandCancellation(CancellationToken);

impl CommandCancellation {
    pub fn new() -> Self {
        Self(CancellationToken::new())
    }

    pub fn cancel(&self) {
        self.0.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Wait until graceful shutdown starts cancelling command work.
    pub async fn cancelled(&self) {
        self.0.cancelled().await;
    }

    /// Stop an invocation before its next externally visible operation.
    pub fn check(&self) -> std::result::Result<(), CommandCancelled> {
        if self.is_cancelled() {
            Err(CommandCancelled)
        } else {
            Ok(())
        }
    }
}

/// Operation-boundary cancellation returned by a command that stopped cleanly.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("command cancelled")]
pub struct CommandCancelled;

/// Stable identity used by per-user admission counters.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum UserKey {
    TwitchId(String),
    Login(String),
}

impl UserKey {
    /// Prefer Twitch's immutable user ID and fall back to the normalized login.
    pub fn from_privmsg(privmsg: &PrivmsgMessage) -> std::result::Result<Self, UserKeyError> {
        let id = privmsg.sender.id.trim();
        if !id.is_empty() {
            return Ok(Self::TwitchId(id.to_owned()));
        }

        let login = normalize_username(privmsg.sender.login.trim());
        if login.is_empty() {
            Err(UserKeyError)
        } else {
            Ok(Self::Login(login))
        }
    }
}

impl fmt::Display for UserKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TwitchId(id) => write!(f, "twitch:{id}"),
            Self::Login(login) => write!(f, "login:{login}"),
        }
    }
}

/// A PRIVMSG has neither a usable user ID nor login.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("command sender has neither a Twitch user ID nor a login")]
pub struct UserKeyError;

/// Fully owned data available for one command execution.
#[derive(Clone)]
pub struct CommandContext<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    /// The complete chat message that triggered the command.
    pub privmsg: PrivmsgMessage,
    /// Shared sanitizing sender owned by the process composition seam.
    pub sender: Arc<ChatSender<T, L>>,
    /// The first word that matched the command.
    pub trigger: String,
    /// Remaining words after the command name.
    pub args: Vec<String>,
    /// Lifecycle cancellation derived from Candidate 08.
    pub cancellation: CommandCancellation,
    /// Stable key used for admission and tracing.
    pub user_key: UserKey,
}

/// A resolved command ID and its owned execution context.
pub struct CommandInvocation<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    pub command_id: CommandId,
    pub context: CommandContext<T, L>,
}

impl<T, L> CommandInvocation<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    /// Build an invocation after the caller has resolved the canonical ID.
    pub fn new(
        command_id: CommandId,
        privmsg: PrivmsgMessage,
        sender: Arc<ChatSender<T, L>>,
        trigger: String,
        args: Vec<String>,
        cancellation: CommandCancellation,
    ) -> std::result::Result<Self, UserKeyError> {
        let user_key = UserKey::from_privmsg(&privmsg)?;
        Ok(Self {
            command_id,
            context: CommandContext {
                privmsg,
                sender,
                trigger,
                args,
                cancellation,
                user_key,
            },
        })
    }

    async fn reject_capacity(self, reason: CapacityRejection) {
        reject_capacity(&self, reason).await;
    }
}

/// Result of one admitted command task.
#[derive(Debug)]
pub struct TaskCompletion {
    pub command_id: CommandId,
    pub user_key: UserKey,
    pub result: TaskResult,
}

/// Command-local completion state. Errors other than cancellation do not fail
/// the manager.
#[derive(Debug)]
pub enum TaskResult {
    Succeeded,
    Failed(eyre::Report),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapacityRejection {
    AdmissionChannel,
    PendingQueue,
    UserPending,
    CommandPending,
}

impl CapacityRejection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AdmissionChannel => "admission_channel_full",
            Self::PendingQueue => "pending_queue_full",
            Self::UserPending => "user_pending_full",
            Self::CommandPending => "command_pending_full",
        }
    }
}

async fn reject_capacity<T, L>(invocation: &CommandInvocation<T, L>, reason: CapacityRejection)
where
    T: Transport,
    L: LoginCredentials,
{
    warn!(
        command_id = %invocation.command_id,
        execution_class = ?ExecutionClass::Background,
        trigger = %invocation.context.trigger,
        user_key = %invocation.context.user_key,
        reason = reason.as_str(),
        CAPACITY_REJECTION_MESSAGE,
        "command invocation rejected for capacity"
    );
    invocation
        .context
        .sender
        .reply(&invocation.context.privmsg, CAPACITY_REJECTION_MESSAGE)
        .await;
}

/// Non-blocking send side of the bounded ingress-to-manager handoff.
pub struct CommandAdmissionSender<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    sender: mpsc::Sender<CommandInvocation<T, L>>,
}

impl<T, L> CommandAdmissionSender<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    /// Try to hand off without waiting for queue capacity. A full channel sends
    /// the exact generic busy response through the invocation's `ChatSender`.
    pub async fn try_admit(
        &self,
        invocation: CommandInvocation<T, L>,
    ) -> std::result::Result<(), CommandAdmissionError> {
        match self.sender.try_send(invocation) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(invocation)) => {
                invocation
                    .reject_capacity(CapacityRejection::AdmissionChannel)
                    .await;
                Err(CommandAdmissionError::Capacity)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(CommandAdmissionError::Closed),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

/// Receiving half owned by `CommandExecutionManager`.
pub struct CommandAdmissionReceiver<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    receiver: mpsc::Receiver<CommandInvocation<T, L>>,
}

impl<T, L> CommandAdmissionReceiver<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    async fn recv(&mut self) -> Option<CommandInvocation<T, L>> {
        self.receiver.recv().await
    }

    pub fn close(&mut self) {
        self.receiver.close();
    }
}

/// Why an invocation could not cross the admission boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandAdmissionError {
    Capacity,
    Closed,
}

/// Create the production bounded admission channel.
pub fn command_admission_channel<T, L>()
-> (CommandAdmissionSender<T, L>, CommandAdmissionReceiver<T, L>)
where
    T: Transport,
    L: LoginCredentials,
{
    command_admission_channel_with_capacity(COMMAND_ADMISSION_CAPACITY)
}

fn command_admission_channel_with_capacity<T, L>(
    capacity: usize,
) -> (CommandAdmissionSender<T, L>, CommandAdmissionReceiver<T, L>)
where
    T: Transport,
    L: LoginCredentials,
{
    assert!(capacity > 0, "command admission capacity must be non-zero");
    let (sender, receiver) = mpsc::channel(capacity);
    (
        CommandAdmissionSender { sender },
        CommandAdmissionReceiver { receiver },
    )
}

#[derive(Clone, Copy)]
struct ExecutionLimits {
    max_active: usize,
    max_pending: usize,
    max_active_per_user: usize,
    max_pending_per_user: usize,
    max_active_per_command: usize,
    max_pending_per_command: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_active: MAX_ACTIVE_BACKGROUND_COMMANDS,
            max_pending: MAX_PENDING_BACKGROUND_COMMANDS,
            max_active_per_user: MAX_ACTIVE_COMMANDS_PER_USER,
            max_pending_per_user: MAX_PENDING_COMMANDS_PER_USER,
            max_active_per_command: MAX_ACTIVE_COMMANDS_PER_COMMAND,
            max_pending_per_command: MAX_PENDING_COMMANDS_PER_COMMAND,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct UserCounters {
    active: usize,
    pending: usize,
}

/// Concrete bounded manager for accepted background command invocations.
pub struct CommandExecutionManager<T, L>
where
    T: Transport,
    L: LoginCredentials,
{
    admission: CommandAdmissionReceiver<T, L>,
    registry: Arc<CommandRegistry<T, L>>,
    limits: ExecutionLimits,
    pending: VecDeque<CommandInvocation<T, L>>,
    users: HashMap<UserKey, UserCounters>,
    active_per_command: HashMap<CommandId, usize>,
    login_ids: HashMap<String, String>,
    active: usize,
    task_group: CommandTaskGroup,
}

impl<T, L> CommandExecutionManager<T, L>
where
    T: Transport + Send + Sync + 'static,
    L: LoginCredentials + Send + Sync + 'static,
{
    pub fn new(
        admission: CommandAdmissionReceiver<T, L>,
        registry: Arc<CommandRegistry<T, L>>,
    ) -> Self {
        Self::with_limits(admission, registry, ExecutionLimits::default())
    }

    fn with_limits(
        admission: CommandAdmissionReceiver<T, L>,
        registry: Arc<CommandRegistry<T, L>>,
        limits: ExecutionLimits,
    ) -> Self {
        Self {
            admission,
            registry,
            limits,
            pending: VecDeque::with_capacity(limits.max_pending),
            users: HashMap::new(),
            active_per_command: HashMap::new(),
            login_ids: HashMap::new(),
            active: 0,
            task_group: CommandTaskGroup::new(),
        }
    }

    /// A clonable adapter Candidate 08 can join or abort while this manager is
    /// owned by its supervised task.
    pub fn task_group(&self) -> CommandTaskGroup {
        self.task_group.clone()
    }

    /// Run until Candidate 08 cancellation causes ingress to close admission.
    pub async fn run(mut self, cancellation: CommandCancellation) -> Result<()> {
        loop {
            self.refill_pending()?;

            tokio::select! {
                completion = async {
                    if self.task_group.is_empty() {
                        std::future::pending::<Option<Result<TaskCompletion, JoinError>>>().await
                    } else {
                        self.task_group.join_next().await
                    }
                } => {
                    match completion {
                        Some(Ok(completion)) => self.finish(completion)?,
                        Some(Err(join_error)) => {
                            if join_error.is_panic() {
                                error!(error = %join_error, "command task panicked");
                                return self.fail(ExecutionError::CommandPanicked).await;
                            }
                            if cancellation.is_cancelled() {
                                debug!(error = %join_error, "command task aborted during shutdown");
                            } else {
                                error!(error = %join_error, "command task was aborted unexpectedly");
                                return self.fail(ExecutionError::CommandAborted).await;
                            }
                        }
                        None => {}
                    }
                }
                incoming = self.admission.recv() => {
                    let Some(invocation) = incoming else {
                        if !cancellation.is_cancelled() {
                            error!("command admission channel closed while running");
                            return self.fail(ExecutionError::AdmissionClosedWhileRunning).await;
                        }
                        break;
                    };

                    self.admit(invocation).await?;
                    self.refill_pending()?;
                }
            }
        }

        info!(
            pending_discarded = self.pending.len(),
            active = self.active,
            "command execution manager draining"
        );
        self.discard_pending();
        while !self.task_group.is_empty() {
            match self.task_group.join_next().await {
                Some(Ok(completion)) => self.finish(completion)?,
                Some(Err(join_error)) if join_error.is_cancelled() => {}
                Some(Err(join_error)) => {
                    error!(error = %join_error, "command task panicked during shutdown");
                    return Err(ExecutionError::CommandPanicked.into());
                }
                None => break,
            }
        }
        self.task_group.join_all().await?;
        info!("command execution manager drained");
        Ok(())
    }

    async fn fail(&self, error: ExecutionError) -> Result<()> {
        if let Err(group_error) = self.task_group.abort_and_join().await {
            error!(error = %group_error, "command task group failed while handling manager error");
        }
        Err(error.into())
    }

    async fn admit(&mut self, invocation: CommandInvocation<T, L>) -> Result<()> {
        let entry = self
            .registry
            .get(&invocation.command_id)
            .ok_or(ExecutionError::UnknownCommandId(invocation.command_id))?;
        if entry.execution_policy() != ExecutionClass::Background {
            return Err(ExecutionError::InlineCommandAdmitted(invocation.command_id).into());
        }
        let command = entry.command().clone();

        if self.can_start(&invocation) {
            self.record_identity(&invocation.context);
            self.trace_accepted(&invocation);
            self.start(command, invocation);
            return Ok(());
        }

        let rejection = if self.pending.len() >= self.limits.max_pending {
            Some(CapacityRejection::PendingQueue)
        } else if self.per_user_pending(&invocation.context.user_key)
            >= self.limits.max_pending_per_user
        {
            Some(CapacityRejection::UserPending)
        } else if self.per_command_pending(&invocation.command_id)
            >= self.limits.max_pending_per_command
        {
            Some(CapacityRejection::CommandPending)
        } else {
            None
        };

        if let Some(reason) = rejection {
            warn!(
                command_id = %invocation.command_id,
                execution_class = ?ExecutionClass::Background,
                trigger = %invocation.context.trigger,
                user_key = %invocation.context.user_key,
                active = self.active,
                pending = self.pending.len(),
                per_command_active = self.per_command_active(&invocation.command_id),
                per_user_active = self.per_user_active(&invocation.context.user_key),
                reason = reason.as_str(),
                "command invocation rejected for capacity"
            );
            invocation
                .context
                .sender
                .reply(&invocation.context.privmsg, CAPACITY_REJECTION_MESSAGE)
                .await;
            return Ok(());
        }

        self.record_identity(&invocation.context);
        self.trace_accepted(&invocation);
        self.increment_pending(&invocation.command_id, &invocation.context.user_key);
        self.pending.push_back(invocation);
        Ok(())
    }

    fn trace_accepted(&self, invocation: &CommandInvocation<T, L>) {
        info!(
            command_id = %invocation.command_id,
            execution_class = ?ExecutionClass::Background,
            trigger = %invocation.context.trigger,
            user_key = %invocation.context.user_key,
            active = self.active,
            pending = self.pending.len(),
            per_command_active = self.per_command_active(&invocation.command_id),
            per_user_active = self.per_user_active(&invocation.context.user_key),
            "command invocation accepted"
        );
    }

    fn refill_pending(&mut self) -> Result<()> {
        while self.active < self.limits.max_active {
            let Some(invocation) = self.pending.front() else {
                break;
            };
            if !self.can_start(invocation) {
                break;
            }

            let invocation = self.pending.pop_front().expect("front was present");
            let entry = self
                .registry
                .get(&invocation.command_id)
                .ok_or(ExecutionError::UnknownCommandId(invocation.command_id))?;
            let command = entry.command().clone();
            self.start(command, invocation);
        }
        Ok(())
    }

    fn start(&mut self, command: Arc<dyn Command<T, L>>, invocation: CommandInvocation<T, L>) {
        let command_id = invocation.command_id;
        let user_key = invocation.context.user_key.clone();
        self.decrement_pending(&command_id, &user_key);
        self.increment_active(&command_id, &user_key);
        debug!(
            command_id = %command_id,
            execution_class = ?ExecutionClass::Background,
            trigger = %invocation.context.trigger,
            user_key = %user_key,
            active = self.active,
            pending = self.pending.len(),
            per_command_active = self.per_command_active(&command_id),
            per_user_active = self.per_user_active(&user_key),
            "command invocation started"
        );
        self.task_group.spawn_command(command, invocation);
    }

    fn finish(&mut self, completion: TaskCompletion) -> Result<()> {
        let TaskCompletion {
            command_id,
            user_key,
            result,
        } = completion;
        self.decrement_active(&command_id, &user_key);

        match result {
            TaskResult::Succeeded => {
                info!(
                    command_id = %command_id,
                    user_key = %user_key,
                    active = self.active,
                    pending = self.pending.len(),
                    "command invocation completed"
                );
            }
            TaskResult::Failed(error) => {
                error!(
                    command_id = %command_id,
                    user_key = %user_key,
                    error = ?error,
                    active = self.active,
                    pending = self.pending.len(),
                    "command invocation failed"
                );
            }
            TaskResult::Cancelled => {
                info!(
                    command_id = %command_id,
                    user_key = %user_key,
                    active = self.active,
                    pending = self.pending.len(),
                    "command invocation cancelled"
                );
            }
        }
        Ok(())
    }

    fn discard_pending(&mut self) {
        while let Some(invocation) = self.pending.pop_front() {
            self.decrement_pending(&invocation.command_id, &invocation.context.user_key);
        }
    }

    fn can_start(&self, invocation: &CommandInvocation<T, L>) -> bool {
        self.active < self.limits.max_active
            && self.per_user_active(&invocation.context.user_key) < self.limits.max_active_per_user
            && self.per_command_active(&invocation.command_id) < self.limits.max_active_per_command
    }

    fn per_user_active(&self, key: &UserKey) -> usize {
        self.users.get(key).map_or(0, |counts| counts.active)
    }

    fn per_user_pending(&self, key: &UserKey) -> usize {
        self.users.get(key).map_or(0, |counts| counts.pending)
    }

    fn per_command_active(&self, id: &CommandId) -> usize {
        self.active_per_command.get(id).copied().unwrap_or(0)
    }

    fn per_command_pending(&self, id: &CommandId) -> usize {
        self.pending
            .iter()
            .filter(|invocation| invocation.command_id == *id)
            .count()
    }

    fn increment_active(&mut self, id: &CommandId, key: &UserKey) {
        self.active += 1;
        *self.active_per_command.entry(*id).or_default() += 1;
        self.users.entry(key.clone()).or_default().active += 1;
    }

    fn decrement_active(&mut self, id: &CommandId, key: &UserKey) {
        self.active = self.active.saturating_sub(1);
        let remove_command = if let Some(count) = self.active_per_command.get_mut(id) {
            *count = count.checked_sub(1).unwrap_or_else(|| {
                panic!("active command counter released below zero");
            });
            *count == 0
        } else {
            panic!("active command counter is missing")
        };
        if remove_command {
            self.active_per_command.remove(id);
        }
        let remove_user = if let Some(counts) = self.users.get_mut(key) {
            counts.active = counts.active.saturating_sub(1);
            counts.active == 0 && counts.pending == 0
        } else {
            false
        };
        if remove_user {
            self.users.remove(key);
        }
    }

    fn increment_pending(&mut self, id: &CommandId, key: &UserKey) {
        self.users.entry(key.clone()).or_default().pending += 1;
        debug!(
            command_id = %id,
            execution_class = ?ExecutionClass::Background,
            user_key = %key,
            active = self.active,
            pending = self.pending.len() + 1,
            per_command_active = self.per_command_active(id),
            per_user_active = self.per_user_active(key),
            "command invocation queued"
        );
    }

    fn decrement_pending(&mut self, _id: &CommandId, key: &UserKey) {
        let remove_user = if let Some(counts) = self.users.get_mut(key) {
            counts.pending = counts.pending.saturating_sub(1);
            counts.active == 0 && counts.pending == 0
        } else {
            false
        };
        if remove_user {
            self.users.remove(key);
        }
    }

    fn record_identity(&mut self, context: &CommandContext<T, L>) {
        let id = context.privmsg.sender.id.trim();
        if id.is_empty() {
            return;
        }
        let login = normalize_username(context.privmsg.sender.login.trim());
        if login.is_empty() {
            return;
        }
        if let Some(previous) = self.login_ids.get(&login)
            && previous != id
        {
            warn!(
                login,
                previous_user_id = previous,
                user_id = id,
                "Twitch login observed with multiple user IDs; using the ID as the stable key"
            );
        }
        self.login_ids.insert(login, id.to_owned());
    }
}

#[derive(Clone)]
pub struct CommandTaskGroup {
    inner: Arc<CommandTaskGroupInner>,
}

struct CommandTaskGroupInner {
    active: Mutex<JoinSet<TaskCompletion>>,
    completed: Semaphore,
    blocking: Mutex<Vec<JoinHandle<()>>>,
}

struct CompletionSignal(Arc<CommandTaskGroupInner>);

impl Drop for CompletionSignal {
    fn drop(&mut self) {
        self.0.completed.add_permits(1);
    }
}

impl Default for CommandTaskGroup {
    fn default() -> Self {
        Self {
            inner: Arc::new(CommandTaskGroupInner {
                active: Mutex::new(JoinSet::new()),
                completed: Semaphore::new(0),
                blocking: Mutex::new(Vec::new()),
            }),
        }
    }
}

impl std::fmt::Debug for CommandTaskGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandTaskGroup")
            .field("active", &self.active_len())
            .field("blocking", &self.blocking_len())
            .finish()
    }
}

impl CommandTaskGroup {
    pub fn new() -> Self {
        Self::default()
    }

    fn spawn_command<T, L>(
        &self,
        command: Arc<dyn Command<T, L>>,
        invocation: CommandInvocation<T, L>,
    ) where
        T: Transport + Send + Sync + 'static,
        L: LoginCredentials + Send + Sync + 'static,
    {
        let command_id = invocation.command_id;
        let user_key = invocation.context.user_key.clone();
        let completion_signal = CompletionSignal(self.inner.clone());
        let task = async move {
            let _completion_signal = completion_signal;
            let result = match command.execute(invocation.context).await {
                Ok(()) => TaskResult::Succeeded,
                Err(error) if error.downcast_ref::<CommandCancelled>().is_some() => {
                    TaskResult::Cancelled
                }
                Err(error) => TaskResult::Failed(error),
            };
            TaskCompletion {
                command_id,
                user_key,
                result,
            }
        };
        let task = CURRENT_COMMAND_TASK_GROUP.scope(self.clone(), task);
        lock(&self.inner.active).spawn(task);
    }

    /// Register blocking work with this group and return its result receiver.
    /// Dropping or aborting the command parent does not detach the blocking
    /// handle: `join_all` / `abort_and_join` still own and join it.
    pub(crate) fn spawn_blocking<F, R>(&self, task: F) -> oneshot::Receiver<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let handle = tokio::task::spawn_blocking(move || {
            let _ = sender.send(task());
        });
        lock(&self.inner.blocking).push(handle);
        receiver
    }

    pub(crate) fn current() -> Option<Self> {
        CURRENT_COMMAND_TASK_GROUP.try_with(Clone::clone).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.active_len() == 0 && self.blocking_len() == 0
    }

    pub fn active_len(&self) -> usize {
        lock(&self.inner.active).len()
    }

    pub fn blocking_len(&self) -> usize {
        lock(&self.inner.blocking).len()
    }

    async fn join_next(&self) -> Option<Result<TaskCompletion, JoinError>> {
        loop {
            if lock(&self.inner.active).is_empty() {
                return None;
            }
            let permit = self
                .inner
                .completed
                .acquire()
                .await
                .expect("command task semaphore is never closed");
            if let Some(completion) = lock(&self.inner.active).try_join_next() {
                drop(permit);
                return Some(completion);
            }
            drop(permit);
        }
    }

    /// Wait for every active command and application-owned blocking child.
    pub async fn join_all(&self) -> std::result::Result<(), TaskGroupError> {
        let mut command_panicked = false;
        while !lock(&self.inner.active).is_empty() {
            match self.join_next().await {
                Some(Ok(_)) => {}
                Some(Err(error)) if error.is_cancelled() => {}
                Some(Err(_error)) => command_panicked = true,
                None => break,
            }
        }
        let blocking_result = self.join_blocking().await;
        if command_panicked {
            Err(TaskGroupError::CommandPanicked)
        } else {
            blocking_result
        }
    }

    /// Abort dynamic command tasks where cancellation is possible, then join
    /// every task and blocking child. Tokio cannot abort an already-running
    /// blocking closure, so this method may wait for that computation to finish.
    pub async fn abort_and_join(&self) -> std::result::Result<(), TaskGroupError> {
        // Move the set out before awaiting. Holding a std::sync::MutexGuard
        // across an await would make the manager future non-Send and could
        // block its concurrent completion poller during deadline teardown.
        let mut active = {
            let mut guard = lock(&self.inner.active);
            std::mem::take(&mut *guard)
        };
        active.abort_all();
        let mut command_panicked = false;
        while let Some(result) = active.join_next().await {
            if let Err(error) = result
                && error.is_panic()
            {
                command_panicked = true;
            }
        }
        let blocking_result = self.join_blocking().await;
        if command_panicked {
            Err(TaskGroupError::CommandPanicked)
        } else {
            blocking_result
        }
    }

    async fn join_blocking(&self) -> std::result::Result<(), TaskGroupError> {
        let handles = std::mem::take(&mut *lock(&self.inner.blocking));
        let mut blocking_panicked = false;
        for handle in handles {
            if handle.await.is_err() {
                blocking_panicked = true;
            }
        }
        if blocking_panicked {
            Err(TaskGroupError::BlockingChildPanicked)
        } else {
            Ok(())
        }
    }
}

tokio::task_local! {
    static CURRENT_COMMAND_TASK_GROUP: CommandTaskGroup;
}

/// Failure while explicitly joining the command task group.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TaskGroupError {
    #[error("a command task panicked")]
    CommandPanicked,
    #[error("an application-owned blocking command child panicked")]
    BlockingChildPanicked,
}

/// Manager invariant or teardown failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ExecutionError {
    #[error("admission channel closed while command execution was running")]
    AdmissionClosedWhileRunning,
    #[error("unknown command ID `{0}`")]
    UnknownCommandId(CommandId),
    #[error("inline command `{0}` was admitted to the background manager")]
    InlineCommandAdmitted(CommandId),
    #[error("a command task panicked")]
    CommandPanicked,
    #[error("a command task was aborted while execution was running")]
    CommandAborted,
}

impl From<TaskGroupError> for ExecutionError {
    fn from(_: TaskGroupError) -> Self {
        Self::CommandPanicked
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::TryFrom;
    use std::fmt;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};

    use async_trait::async_trait;
    use either::Either;
    use futures_util::{sink::Sink, stream::Pending};
    use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, mpsc};
    use tokio::time::{Duration, timeout};
    use twitch_irc::login::StaticLoginCredentials;
    use twitch_irc::message::{AsRawIRC, IRCMessage, IRCParseError, PrivmsgMessage};
    use twitch_irc::transport::Transport;
    use twitch_irc::{ClientConfig, TwitchIRCClient};

    use super::*;
    use crate::commands::{Command, CommandRegistry, ExecutionClass};

    type TestTransport = CaptureTransport;
    type TestLogin = StaticLoginCredentials;
    type TestChat = ChatSender<TestTransport, TestLogin>;

    #[derive(Debug)]
    struct CaptureTransport {
        output: mpsc::UnboundedSender<String>,
    }

    #[derive(Debug)]
    struct CaptureConnectError;

    impl fmt::Display for CaptureConnectError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("capture transport connection error")
        }
    }

    impl std::error::Error for CaptureConnectError {}

    struct CaptureSink {
        output: mpsc::UnboundedSender<String>,
    }

    impl Sink<IRCMessage> for CaptureSink {
        type Error = io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: IRCMessage) -> Result<(), Self::Error> {
            self.output
                .send(item.as_raw_irc())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "capture receiver closed"))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[async_trait]
    impl Transport for CaptureTransport {
        type ConnectError = CaptureConnectError;
        type IncomingError = io::Error;
        type OutgoingError = io::Error;
        type Incoming = Pending<Result<IRCMessage, Either<io::Error, IRCParseError>>>;
        type Outgoing = CaptureSink;

        async fn new() -> Result<Self, Self::ConnectError> {
            let output = CAPTURE_OUTPUTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or(CaptureConnectError)?;
            Ok(Self { output })
        }

        fn split(self) -> (Self::Incoming, Self::Outgoing) {
            (
                futures_util::stream::pending(),
                CaptureSink {
                    output: self.output,
                },
            )
        }
    }

    // `Transport::new` has no arguments. Keep one capture sender queued per
    // test and hold TEST_LOCK until the client has consumed it; this makes
    // parallel unit tests independent without sharing an IRC connection.
    static CAPTURE_OUTPUTS: Mutex<VecDeque<mpsc::UnboundedSender<String>>> =
        Mutex::new(VecDeque::new());
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestSender {
        sender: Arc<TestChat>,
        _output: mpsc::UnboundedReceiver<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn test_sender() -> TestSender {
        let lock = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (output_tx, output) = mpsc::unbounded_channel();
        CAPTURE_OUTPUTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(output_tx);
        let config = ClientConfig::new_simple(StaticLoginCredentials::new(
            "bot".to_owned(),
            Some("token".to_owned()),
        ));
        let (incoming, client) = TwitchIRCClient::<TestTransport, TestLogin>::new(config);
        drop(incoming);
        TestSender {
            sender: ChatSender::new(Arc::new(client)),
            _output: output,
            _lock: lock,
        }
    }

    fn privmsg(login: &str, id: &str) -> PrivmsgMessage {
        let raw = format!(
            "@badge-info=;badges=;color=;display-name={login};emotes=;flags=;id=msg-1;mod=0;room-id=1;subscriber=0;tmi-sent-ts=1700000000000;user-id={id};user-type= :{login}!{login}@{login}.tmi.twitch.tv PRIVMSG #test :hello"
        );
        PrivmsgMessage::try_from(IRCMessage::parse(&raw).unwrap()).unwrap()
    }

    struct TestCommand {
        id: CommandId,
        name: &'static str,
        policy: ExecutionClass,
        started: Arc<AsyncMutex<Vec<String>>>,
        started_notify: Arc<Notify>,
        gate: Option<Arc<Semaphore>>,
        fail: bool,
        panic: bool,
        observe_cancellation: bool,
    }

    impl TestCommand {
        fn background(
            id: &'static str,
            name: &'static str,
            started: Arc<AsyncMutex<Vec<String>>>,
            started_notify: Arc<Notify>,
            gate: Option<Arc<Semaphore>>,
        ) -> Arc<dyn Command<TestTransport, TestLogin>> {
            Arc::new(Self {
                id: CommandId::new(id).unwrap(),
                name,
                policy: ExecutionClass::Background,
                started,
                started_notify,
                gate,
                fail: false,
                panic: false,
                observe_cancellation: false,
            })
        }

        fn failing(
            id: &'static str,
            name: &'static str,
            started: Arc<AsyncMutex<Vec<String>>>,
            started_notify: Arc<Notify>,
        ) -> Arc<dyn Command<TestTransport, TestLogin>> {
            Arc::new(Self {
                id: CommandId::new(id).unwrap(),
                name,
                policy: ExecutionClass::Background,
                started,
                started_notify,
                gate: None,
                fail: true,
                panic: false,
                observe_cancellation: false,
            })
        }

        fn panicking(
            id: &'static str,
            name: &'static str,
            started: Arc<AsyncMutex<Vec<String>>>,
            started_notify: Arc<Notify>,
        ) -> Arc<dyn Command<TestTransport, TestLogin>> {
            Arc::new(Self {
                id: CommandId::new(id).unwrap(),
                name,
                policy: ExecutionClass::Background,
                started,
                started_notify,
                gate: None,
                fail: false,
                panic: true,
                observe_cancellation: false,
            })
        }

        fn cancelling(
            id: &'static str,
            name: &'static str,
            started: Arc<AsyncMutex<Vec<String>>>,
            started_notify: Arc<Notify>,
        ) -> Arc<dyn Command<TestTransport, TestLogin>> {
            Arc::new(Self {
                id: CommandId::new(id).unwrap(),
                name,
                policy: ExecutionClass::Background,
                started,
                started_notify,
                gate: None,
                fail: false,
                panic: false,
                observe_cancellation: true,
            })
        }
    }

    #[async_trait]
    impl Command<TestTransport, TestLogin> for TestCommand {
        fn id(&self) -> CommandId {
            self.id
        }

        fn name(&self) -> &str {
            self.name
        }

        fn execution_policy(&self) -> ExecutionClass {
            self.policy
        }

        async fn execute(&self, ctx: CommandContext<TestTransport, TestLogin>) -> Result<()> {
            self.started.lock().await.push(ctx.trigger.clone());
            self.started_notify.notify_one();
            if let Some(gate) = &self.gate {
                gate.acquire().await.expect("gate remains open").forget();
            }
            if self.panic {
                panic!("intentional command panic");
            }
            if self.observe_cancellation && ctx.cancellation.is_cancelled() {
                return Err(CommandCancelled.into());
            }
            if self.fail {
                return Err(eyre::eyre!("intentional command error"));
            }
            Ok(())
        }
    }

    fn registry(
        commands: Vec<Arc<dyn Command<TestTransport, TestLogin>>>,
    ) -> Arc<CommandRegistry<TestTransport, TestLogin>> {
        let catch_all: Arc<dyn Command<TestTransport, TestLogin>> = Arc::new(TestCommand {
            id: CommandId::new("ping_trigger").unwrap(),
            name: "!<ping>",
            policy: ExecutionClass::Inline,
            started: Arc::new(AsyncMutex::new(Vec::new())),
            started_notify: Arc::new(Notify::new()),
            gate: None,
            fail: false,
            panic: false,
            observe_cancellation: false,
        });
        Arc::new(
            CommandRegistry::from_commands(commands.into_iter().chain(std::iter::once(catch_all)))
                .unwrap(),
        )
    }

    fn invocation(
        sender: Arc<TestChat>,
        id: &'static str,
        user: &str,
        trigger: &str,
    ) -> CommandInvocation<TestTransport, TestLogin> {
        CommandInvocation::new(
            CommandId::new(id).unwrap(),
            privmsg(user, &format!("id-{user}")),
            sender,
            trigger.to_owned(),
            vec![trigger.to_owned()],
            CommandCancellation::new(),
        )
        .unwrap()
    }

    fn make_manager(
        registry: Arc<CommandRegistry<TestTransport, TestLogin>>,
        limits: ExecutionLimits,
    ) -> (
        CommandAdmissionSender<TestTransport, TestLogin>,
        CommandExecutionManager<TestTransport, TestLogin>,
    ) {
        let (sender, receiver) = command_admission_channel_with_capacity(8);
        (
            sender,
            CommandExecutionManager::with_limits(receiver, registry, limits),
        )
    }

    async fn wait_started(notify: &Notify) {
        timeout(Duration::from_secs(1), notify.notified())
            .await
            .expect("command did not start");
    }

    #[tokio::test]
    async fn user_key_prefers_twitch_id_and_normalizes_login_fallback() {
        let id_message = privmsg("Alice", "123");
        assert_eq!(
            UserKey::from_privmsg(&id_message).unwrap(),
            UserKey::TwitchId("123".to_owned())
        );

        let mut fallback = privmsg("Alice", "placeholder");
        fallback.sender.id.clear();
        fallback.sender.login = "@ALICE".to_owned();
        assert_eq!(
            UserKey::from_privmsg(&fallback).unwrap(),
            UserKey::Login("alice".to_owned())
        );

        fallback.sender.login.clear();
        assert_eq!(UserKey::from_privmsg(&fallback), Err(UserKeyError));
    }

    #[tokio::test]
    async fn busy_response_is_exactly_the_capacity_message() {
        assert_eq!(CAPACITY_REJECTION_MESSAGE, "zu viel zu tun");
    }

    #[tokio::test]
    async fn full_admission_channel_is_rejected_without_blocking() {
        let test_sender = test_sender();
        let (sender, _receiver) =
            command_admission_channel_with_capacity::<TestTransport, TestLogin>(1);
        sender
            .sender
            .try_send(invocation(test_sender.sender.clone(), "ai", "one", "first"))
            .unwrap();
        let result = sender
            .try_admit(invocation(
                test_sender.sender.clone(),
                "ai",
                "two",
                "second",
            ))
            .await;
        assert_eq!(result, Err(CommandAdmissionError::Capacity));
        assert_eq!(CAPACITY_REJECTION_MESSAGE, "zu viel zu tun");
    }

    #[tokio::test]
    async fn pending_queue_is_fifo_and_refills_in_order() {
        let test_sender = test_sender();
        let started = Arc::new(AsyncMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let gate = Arc::new(Semaphore::new(0));
        let command = TestCommand::background(
            "ai",
            "!ai",
            started.clone(),
            notify.clone(),
            Some(gate.clone()),
        );
        let limits = ExecutionLimits {
            max_active: 1,
            max_pending: 2,
            max_active_per_user: 10,
            max_pending_per_user: 10,
            max_active_per_command: 10,
            max_pending_per_command: 10,
        };
        let (_sender, mut manager) = make_manager(registry(vec![command]), limits);
        manager
            .admit(invocation(test_sender.sender.clone(), "ai", "one", "one"))
            .await
            .unwrap();
        wait_started(&notify).await;
        manager
            .admit(invocation(test_sender.sender.clone(), "ai", "two", "two"))
            .await
            .unwrap();
        manager
            .admit(invocation(
                test_sender.sender.clone(),
                "ai",
                "three",
                "three",
            ))
            .await
            .unwrap();
        assert_eq!(manager.pending.len(), 2);

        for _ in 0..3 {
            gate.add_permits(1);
            let completion = manager.task_group.join_next().await.unwrap().unwrap();
            manager.finish(completion).unwrap();
            manager.refill_pending().unwrap();
        }
        assert_eq!(*started.lock().await, ["one", "two", "three"]);
    }

    #[tokio::test]
    async fn manager_rejects_global_pending_capacity_without_execution() {
        let test_sender = test_sender();
        let started = Arc::new(AsyncMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let gate = Arc::new(Semaphore::new(0));
        let command = TestCommand::background(
            "ai",
            "!ai",
            started.clone(),
            notify.clone(),
            Some(gate.clone()),
        );
        let limits = ExecutionLimits {
            max_active: 1,
            max_pending: 1,
            max_active_per_user: 10,
            max_pending_per_user: 10,
            max_active_per_command: 10,
            max_pending_per_command: 10,
        };
        let (_sender, mut manager) = make_manager(registry(vec![command]), limits);
        manager
            .admit(invocation(test_sender.sender.clone(), "ai", "one", "one"))
            .await
            .unwrap();
        wait_started(&notify).await;
        manager
            .admit(invocation(test_sender.sender.clone(), "ai", "two", "two"))
            .await
            .unwrap();
        manager
            .admit(invocation(
                test_sender.sender.clone(),
                "ai",
                "three",
                "three",
            ))
            .await
            .unwrap();
        assert_eq!(manager.pending.len(), 1);
        assert_eq!(*started.lock().await, ["one"]);
        gate.add_permits(1);
        manager.task_group.abort_and_join().await.unwrap();
    }

    #[tokio::test]
    async fn per_user_and_per_command_limits_queue_then_release() {
        let test_sender = test_sender();
        let started = Arc::new(AsyncMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let gate = Arc::new(Semaphore::new(0));
        let command = TestCommand::background(
            "ai",
            "!ai",
            started.clone(),
            notify.clone(),
            Some(gate.clone()),
        );
        let limits = ExecutionLimits {
            max_active: 4,
            max_pending: 4,
            max_active_per_user: 1,
            max_pending_per_user: 1,
            max_active_per_command: 1,
            max_pending_per_command: 1,
        };
        let (_sender, mut manager) = make_manager(registry(vec![command]), limits);
        manager
            .admit(invocation(
                test_sender.sender.clone(),
                "ai",
                "same",
                "first",
            ))
            .await
            .unwrap();
        wait_started(&notify).await;
        manager
            .admit(invocation(
                test_sender.sender.clone(),
                "ai",
                "same",
                "second",
            ))
            .await
            .unwrap();
        assert_eq!(manager.pending.len(), 1);
        assert_eq!(
            manager.per_user_active(&UserKey::TwitchId("id-same".to_owned())),
            1
        );
        assert_eq!(
            manager.per_command_active(&CommandId::new("ai").unwrap()),
            1
        );
        gate.add_permits(2);
        timeout(Duration::from_secs(1), async {
            while !manager.task_group.is_empty() {
                let completion = manager.task_group.join_next().await.unwrap().unwrap();
                manager.finish(completion).unwrap();
                manager.refill_pending().unwrap();
            }
        })
        .await
        .unwrap();
        assert!(manager.users.is_empty());
        assert_eq!(manager.active, 0);
    }

    #[tokio::test]
    async fn success_error_and_cancellation_release_counters() {
        for fail in [false, true] {
            let test_sender = test_sender();
            let started = Arc::new(AsyncMutex::new(Vec::new()));
            let notify = Arc::new(Notify::new());
            let command = if fail {
                TestCommand::failing("ai", "!ai", started.clone(), notify.clone())
            } else {
                TestCommand::background("ai", "!ai", started.clone(), notify.clone(), None)
            };
            let (_sender, mut manager) =
                make_manager(registry(vec![command]), ExecutionLimits::default());
            manager
                .admit(invocation(test_sender.sender.clone(), "ai", "user", "done"))
                .await
                .unwrap();
            timeout(Duration::from_secs(1), async {
                let completion = manager.task_group.join_next().await.unwrap().unwrap();
                manager.finish(completion).unwrap();
            })
            .await
            .unwrap();
            assert_eq!(manager.active, 0);
            assert!(manager.users.is_empty());
        }

        let test_sender = test_sender();
        let started = Arc::new(AsyncMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let command = TestCommand::cancelling("ai", "!ai", started, notify);
        let (_sender, mut manager) =
            make_manager(registry(vec![command]), ExecutionLimits::default());
        let invocation = invocation(test_sender.sender.clone(), "ai", "user", "cancelled");
        invocation.context.cancellation.cancel();
        manager.admit(invocation).await.unwrap();
        let completion = manager.task_group.join_next().await.unwrap().unwrap();
        manager.finish(completion).unwrap();
        assert_eq!(manager.active, 0);
        assert!(manager.users.is_empty());
    }

    #[tokio::test]
    async fn shutdown_discard_releases_pending_counters() {
        let test_sender = test_sender();
        let started = Arc::new(AsyncMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let gate = Arc::new(Semaphore::new(0));
        let command =
            TestCommand::background("ai", "!ai", started, notify.clone(), Some(gate.clone()));
        let limits = ExecutionLimits {
            max_active: 1,
            max_pending: 1,
            max_active_per_user: 10,
            max_pending_per_user: 10,
            max_active_per_command: 10,
            max_pending_per_command: 10,
        };
        let (_sender, mut manager) = make_manager(registry(vec![command]), limits);
        manager
            .admit(invocation(
                test_sender.sender.clone(),
                "ai",
                "one",
                "active",
            ))
            .await
            .unwrap();
        wait_started(&notify).await;
        manager
            .admit(invocation(
                test_sender.sender.clone(),
                "ai",
                "two",
                "discard",
            ))
            .await
            .unwrap();
        assert_eq!(manager.pending.len(), 1);
        manager.discard_pending();
        assert!(manager.pending.is_empty());

        gate.add_permits(1);
        let completion = manager.task_group.join_next().await.unwrap().unwrap();
        manager.finish(completion).unwrap();
        assert!(manager.users.is_empty());
        assert_eq!(manager.active, 0);
    }

    #[tokio::test]
    async fn manager_result_distinguishes_running_close_and_graceful_drain() {
        let _sender_bundle = test_sender();
        let command = TestCommand::background(
            "ai",
            "!ai",
            Arc::new(AsyncMutex::new(Vec::new())),
            Arc::new(Notify::new()),
            None,
        );
        let (sender, manager) = make_manager(registry(vec![command]), ExecutionLimits::default());
        let cancellation = CommandCancellation::new();
        let task = tokio::spawn(manager.run(cancellation.clone()));
        drop(sender);
        let error = task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("closed while"));
        drop(_sender_bundle);

        let _sender_bundle = test_sender();
        let command = TestCommand::background(
            "ai",
            "!ai",
            Arc::new(AsyncMutex::new(Vec::new())),
            Arc::new(Notify::new()),
            None,
        );
        let (sender, manager) = make_manager(registry(vec![command]), ExecutionLimits::default());
        let cancellation = CommandCancellation::new();
        cancellation.cancel();
        drop(_sender_bundle);
        let task = tokio::spawn(manager.run(cancellation));
        drop(sender);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn manager_escalates_unknown_id_and_panicking_tasks() {
        let sender_bundle = test_sender();
        let command = TestCommand::background(
            "ai",
            "!ai",
            Arc::new(AsyncMutex::new(Vec::new())),
            Arc::new(Notify::new()),
            None,
        );
        let (sender, manager) = make_manager(registry(vec![command]), ExecutionLimits::default());
        sender
            .sender
            .try_send(invocation(
                sender_bundle.sender.clone(),
                "not_registered",
                "user",
                "bad",
            ))
            .unwrap();
        drop(sender);
        let error = manager.run(CommandCancellation::new()).await.unwrap_err();
        assert!(error.to_string().contains("unknown command ID"));
        drop(sender_bundle);

        let sender_bundle = test_sender();
        let command = TestCommand::panicking(
            "ai",
            "!ai",
            Arc::new(AsyncMutex::new(Vec::new())),
            Arc::new(Notify::new()),
        );
        let (sender, manager) = make_manager(registry(vec![command]), ExecutionLimits::default());
        sender
            .sender
            .try_send(invocation(
                sender_bundle.sender.clone(),
                "ai",
                "user",
                "panic",
            ))
            .unwrap();
        drop(sender);
        let cancellation = CommandCancellation::new();
        cancellation.cancel();
        let error = manager.run(cancellation).await.unwrap_err();
        assert!(error.to_string().contains("panicked"));
    }

    #[tokio::test]
    async fn task_group_joins_blocking_children_and_aborts_commands() {
        let group = CommandTaskGroup::new();
        let receiver = group.spawn_blocking(|| 7_u8);
        assert_eq!(receiver.await.unwrap(), 7);
        group.join_all().await.unwrap();
        assert!(group.is_empty());

        let started = Arc::new(AsyncMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let gate = Arc::new(Semaphore::new(0));
        let test_sender = test_sender();
        let command = TestCommand::background("ai", "!ai", started, notify.clone(), Some(gate));
        group.spawn_command(
            command,
            invocation(test_sender.sender.clone(), "ai", "user", "blocked"),
        );
        wait_started(&notify).await;
        group.abort_and_join().await.unwrap();
        assert!(group.is_empty());
    }
}
