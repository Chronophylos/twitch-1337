use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use eyre::Result;
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{JoinError, JoinSet};
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
/// Upper bound on a capacity reply attempt before the manager resumes scheduling.
const CAPACITY_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
/// Maximum number of login/ID pairs retained only for discrepancy tracing.
const MAX_TRACKED_LOGIN_IDS: usize = 256;

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
    if tokio::time::timeout(
        CAPACITY_REPLY_TIMEOUT,
        invocation
            .context
            .sender
            .reply(&invocation.context.privmsg, CAPACITY_REJECTION_MESSAGE),
    )
    .await
    .is_err()
    {
        warn!(
            timeout_ms = CAPACITY_REPLY_TIMEOUT.as_millis(),
            "capacity rejection reply did not complete before the scheduling bound"
        );
    }
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
    recent_login_ids: VecDeque<(String, String)>,
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
            recent_login_ids: VecDeque::new(),
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
        if self.task_group.is_closed() {
            return self.fail(ExecutionError::TaskGroupClosed).await;
        }

        loop {
            if let Err(error) = self.refill_pending() {
                return self.fail(error).await;
            }
            if self.task_group.is_closed() {
                return self.fail(ExecutionError::TaskGroupClosed).await;
            }

            tokio::select! {
                biased;
                event = async {
                    if self.task_group.is_empty() {
                        std::future::pending::<TaskGroupEvent>().await
                    } else {
                        self.task_group.join_next_event().await
                    }
                } => {
                    if let Err(error) = self.handle_task_group_event(event) {
                        return self.fail(error).await;
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

                    if let Err(error) = self.admit(invocation).await {
                        return self.fail(error).await;
                    }
                    if let Err(error) = self.refill_pending() {
                        return self.fail(error).await;
                    }
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
            match self.task_group.join_next_event().await {
                TaskGroupEvent::Command(Some(Ok(completion))) => {
                    if let Err(error) = self.finish(completion) {
                        return self.fail(error).await;
                    }
                }
                TaskGroupEvent::Command(Some(Err(join_error))) => {
                    error!(error = %join_error, "command task panicked during shutdown");
                    return self.fail(ExecutionError::CommandPanicked).await;
                }
                TaskGroupEvent::Command(None) => {}
                TaskGroupEvent::Blocking(result) => {
                    if let Err(join_error) = result {
                        let error = if join_error.is_panic() {
                            ExecutionError::BlockingChildPanicked
                        } else {
                            ExecutionError::TaskGroupCleanupFailed
                        };
                        return self.fail(error).await;
                    }
                }
                TaskGroupEvent::Child(result) => {
                    if let Err(join_error) = result {
                        let error = if join_error.is_panic() {
                            ExecutionError::ApplicationChildPanicked
                        } else {
                            ExecutionError::TaskGroupCleanupFailed
                        };
                        return self.fail(error).await;
                    }
                }
                TaskGroupEvent::Closed => {
                    return self.fail(ExecutionError::TaskGroupClosed).await;
                }
            }
        }
        if let Err(group_error) = self.task_group.join_all().await {
            return self.fail(group_error.into()).await;
        }
        info!("command execution manager drained");
        Ok(())
    }

    fn handle_task_group_event(&mut self, event: TaskGroupEvent) -> Result<(), ExecutionError> {
        match event {
            TaskGroupEvent::Command(Some(Ok(completion))) => self.finish(completion),
            TaskGroupEvent::Command(Some(Err(join_error))) if join_error.is_panic() => {
                error!(error = %join_error, "command task panicked");
                Err(ExecutionError::CommandPanicked)
            }
            TaskGroupEvent::Command(Some(Err(join_error))) => {
                error!(error = %join_error, "command task was aborted unexpectedly");
                Err(ExecutionError::CommandAborted)
            }
            TaskGroupEvent::Command(None) => Ok(()),
            TaskGroupEvent::Blocking(Err(join_error)) if join_error.is_panic() => {
                error!(error = %join_error, "application-owned blocking command child panicked");
                Err(ExecutionError::BlockingChildPanicked)
            }
            TaskGroupEvent::Blocking(Err(join_error)) => {
                error!(error = %join_error, "application-owned blocking command child failed");
                Err(ExecutionError::TaskGroupCleanupFailed)
            }
            TaskGroupEvent::Blocking(Ok(())) => Ok(()),
            TaskGroupEvent::Child(Err(join_error)) if join_error.is_panic() => {
                error!(error = %join_error, "application-owned command child panicked");
                Err(ExecutionError::ApplicationChildPanicked)
            }
            TaskGroupEvent::Child(Err(join_error)) => {
                error!(error = %join_error, "application-owned command child failed");
                Err(ExecutionError::TaskGroupCleanupFailed)
            }
            TaskGroupEvent::Child(Ok(())) => Ok(()),
            TaskGroupEvent::Closed => Err(ExecutionError::TaskGroupClosed),
        }
    }

    /// The single invariant/teardown cleanup path. Pending work is discarded,
    /// task admission is closed, and all command/application-owned children are
    /// aborted and joined before the manager returns an error.
    async fn fail(&mut self, error: ExecutionError) -> Result<()> {
        error!(error = %error, "command execution manager failed");
        self.task_group.close();
        self.discard_pending();
        if let Err(group_error) = self.task_group.abort_and_join().await {
            error!(error = %group_error, "command task group cleanup failed");
            return Err(ExecutionError::TaskGroup(group_error).into());
        }
        Err(error.into())
    }

    async fn admit(&mut self, invocation: CommandInvocation<T, L>) -> Result<(), ExecutionError> {
        let entry = self
            .registry
            .get(&invocation.command_id)
            .ok_or(ExecutionError::UnknownCommandId(invocation.command_id))?;
        if entry.execution_policy() != ExecutionClass::Background {
            return Err(ExecutionError::InlineCommandAdmitted(invocation.command_id));
        }
        let command = entry.command().clone();

        if self.can_start(&invocation) {
            self.record_identity(&invocation.context);
            self.trace_accepted(&invocation);
            self.start(command, invocation, false)?;
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
            reject_capacity(&invocation, reason).await;
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

    fn refill_pending(&mut self) -> Result<(), ExecutionError> {
        while self.active < self.limits.max_active {
            let Some(invocation) = self.pending.front() else {
                break;
            };
            if !self.can_start(invocation) {
                break;
            }

            // Resolve before removing the accepted pending invocation. A lookup
            // failure must leave queue/counter ownership intact for cleanup.
            let entry = self
                .registry
                .get(&invocation.command_id)
                .ok_or(ExecutionError::UnknownCommandId(invocation.command_id))?;
            if entry.execution_policy() != ExecutionClass::Background {
                return Err(ExecutionError::InlineCommandAdmitted(invocation.command_id));
            }
            let command = entry.command().clone();
            let invocation = self
                .pending
                .pop_front()
                .expect("front was present after capacity checks");
            self.start(command, invocation, true)?;
        }
        Ok(())
    }

    fn start(
        &mut self,
        command: Arc<dyn Command<T, L>>,
        invocation: CommandInvocation<T, L>,
        was_pending: bool,
    ) -> Result<(), ExecutionError> {
        let command_id = invocation.command_id;
        let user_key = invocation.context.user_key.clone();
        if was_pending {
            self.decrement_pending(&command_id, &user_key);
        }
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
        if let Err(error) = self.task_group.spawn_command(command, invocation) {
            self.decrement_active(&command_id, &user_key);
            return Err(error.into());
        }
        Ok(())
    }

    fn finish(&mut self, completion: TaskCompletion) -> Result<(), ExecutionError> {
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
        self.active = self
            .active
            .checked_sub(1)
            .expect("global active command counter released below zero");
        let remove_command = if let Some(count) = self.active_per_command.get_mut(id) {
            *count = count
                .checked_sub(1)
                .expect("active command counter released below zero");
            *count == 0
        } else {
            panic!("active command counter is missing")
        };
        if remove_command {
            self.active_per_command.remove(id);
        }
        let remove_user = if let Some(counts) = self.users.get_mut(key) {
            counts.active = counts
                .active
                .checked_sub(1)
                .expect("per-user active command counter released below zero");
            counts.active == 0 && counts.pending == 0
        } else {
            panic!("per-user active command counter is missing")
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
            counts.pending = counts
                .pending
                .checked_sub(1)
                .expect("per-user pending command counter released below zero");
            counts.active == 0 && counts.pending == 0
        } else {
            panic!("per-user pending command counter is missing")
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
        if let Some((_, previous)) = self
            .recent_login_ids
            .iter()
            .find(|(known_login, _)| known_login == &login)
        {
            if previous != id {
                warn!(
                    login,
                    previous_user_id = previous,
                    user_id = id,
                    "Twitch login observed with multiple user IDs; using the ID as the stable key"
                );
            }
            return;
        }
        if self.recent_login_ids.len() == MAX_TRACKED_LOGIN_IDS {
            self.recent_login_ids.pop_front();
        }
        self.recent_login_ids.push_back((login, id.to_owned()));
    }
}

#[derive(Clone)]
pub struct CommandTaskGroup {
    inner: Arc<CommandTaskGroupInner>,
}

struct CommandTaskGroupInner {
    tasks: Mutex<CommandTasks>,
    completed: Semaphore,
}

struct CommandTasks {
    active: JoinSet<TaskCompletion>,
    blocking: JoinSet<()>,
    children: JoinSet<()>,
    closed: bool,
}

struct CompletionSignal(Arc<CommandTaskGroupInner>);

enum TaskGroupEvent {
    Command(Option<Result<TaskCompletion, JoinError>>),
    Blocking(Result<(), JoinError>),
    Child(Result<(), JoinError>),
    Closed,
}

impl Drop for CompletionSignal {
    fn drop(&mut self) {
        self.0.completed.add_permits(1);
    }
}

impl Default for CommandTaskGroup {
    fn default() -> Self {
        Self {
            inner: Arc::new(CommandTaskGroupInner {
                tasks: Mutex::new(CommandTasks {
                    active: JoinSet::new(),
                    blocking: JoinSet::new(),
                    children: JoinSet::new(),
                    closed: false,
                }),
                completed: Semaphore::new(0),
            }),
        }
    }
}

impl std::fmt::Debug for CommandTaskGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandTaskGroup")
            .field("active", &self.active_len())
            .field("blocking", &self.blocking_len())
            .field("children", &self.child_len())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl CommandTaskGroup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn_command<T, L>(
        &self,
        command: Arc<dyn Command<T, L>>,
        invocation: CommandInvocation<T, L>,
    ) -> std::result::Result<(), TaskGroupError>
    where
        T: Transport + Send + Sync + 'static,
        L: LoginCredentials + Send + Sync + 'static,
    {
        let command_id = invocation.command_id;
        let user_key = invocation.context.user_key.clone();
        let completion_signal = self.inner.clone();
        let task = async move {
            let _completion_signal = CompletionSignal(completion_signal);
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
        let mut tasks = lock(&self.inner.tasks);
        if tasks.closed {
            return Err(TaskGroupError::Closed);
        }
        tasks.active.spawn(task);
        Ok(())
    }

    /// Register blocking work with this group and return its result receiver.
    /// Dropping or aborting the command parent does not detach the blocking
    /// handle: `join_all` / `abort_and_join` still own and join it.
    pub(crate) fn spawn_blocking<F, R>(
        &self,
        task: F,
    ) -> std::result::Result<oneshot::Receiver<R>, TaskGroupError>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let mut tasks = lock(&self.inner.tasks);
        if tasks.closed {
            return Err(TaskGroupError::Closed);
        }
        let completion_signal = CompletionSignal(self.inner.clone());
        tasks.blocking.spawn_blocking(move || {
            let _completion_signal = completion_signal;
            let _ = sender.send(task());
        });
        Ok(receiver)
    }

    /// Register an asynchronous application-owned child of a command.
    pub(crate) fn spawn_child<F, R>(
        &self,
        task: F,
    ) -> std::result::Result<oneshot::Receiver<R>, TaskGroupError>
    where
        F: std::future::Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let mut tasks = lock(&self.inner.tasks);
        if tasks.closed {
            return Err(TaskGroupError::Closed);
        }
        let completion_signal = CompletionSignal(self.inner.clone());
        tasks.children.spawn(async move {
            let _completion_signal = completion_signal;
            let _ = sender.send(task.await);
        });
        Ok(receiver)
    }

    pub(crate) fn current() -> Option<Self> {
        CURRENT_COMMAND_TASK_GROUP.try_with(Clone::clone).ok()
    }

    #[cfg(test)]
    pub(crate) fn scope<F>(&self, future: F) -> impl std::future::Future<Output = F::Output>
    where
        F: std::future::Future,
    {
        CURRENT_COMMAND_TASK_GROUP.scope(self.clone(), future)
    }

    /// Permanently close admission to command and application-owned child work.
    pub fn close(&self) {
        let mut tasks = lock(&self.inner.tasks);
        if !tasks.closed {
            tasks.closed = true;
            self.inner.completed.add_permits(1);
        }
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.inner.tasks).closed
    }

    pub fn is_empty(&self) -> bool {
        let tasks = lock(&self.inner.tasks);
        tasks.active.is_empty() && tasks.blocking.is_empty() && tasks.children.is_empty()
    }

    pub fn active_len(&self) -> usize {
        lock(&self.inner.tasks).active.len()
    }

    pub fn blocking_len(&self) -> usize {
        lock(&self.inner.tasks).blocking.len()
    }

    pub fn child_len(&self) -> usize {
        lock(&self.inner.tasks).children.len()
    }

    async fn join_next_event(&self) -> TaskGroupEvent {
        loop {
            let (empty, closed) = {
                let tasks = lock(&self.inner.tasks);
                (
                    tasks.active.is_empty()
                        && tasks.blocking.is_empty()
                        && tasks.children.is_empty(),
                    tasks.closed,
                )
            };
            if empty {
                return if closed {
                    TaskGroupEvent::Closed
                } else {
                    std::future::pending().await
                };
            }
            let permit = self
                .inner
                .completed
                .acquire()
                .await
                .expect("command task semaphore is never closed");
            let event = {
                let mut tasks = lock(&self.inner.tasks);
                if let Some(result) = tasks.active.try_join_next() {
                    Some(TaskGroupEvent::Command(Some(result)))
                } else if let Some(result) = tasks.blocking.try_join_next() {
                    Some(TaskGroupEvent::Blocking(result))
                } else {
                    tasks.children.try_join_next().map(TaskGroupEvent::Child)
                }
            };
            drop(permit);
            if let Some(event) = event {
                return event;
            }
        }
    }

    /// Close admission, then wait for every command and application-owned child.
    pub async fn join_all(&self) -> std::result::Result<(), TaskGroupError> {
        let (mut active, mut blocking, mut children) = self.close_and_take();
        Self::join_active(&mut active, false).await?;
        Self::join_blocking(&mut blocking).await?;
        Self::join_children(&mut children, false).await
    }

    /// Close admission, abort dynamic command/application children where
    /// cancellation is possible, then join every task and blocking child. Tokio
    /// cannot abort an already-running blocking closure, so this may wait for it.
    pub async fn abort_and_join(&self) -> std::result::Result<(), TaskGroupError> {
        let (mut active, mut blocking, mut children) = self.close_and_take();
        active.abort_all();
        children.abort_all();
        Self::join_active(&mut active, true).await?;
        Self::join_blocking(&mut blocking).await?;
        Self::join_children(&mut children, true).await
    }

    fn close_and_take(&self) -> (JoinSet<TaskCompletion>, JoinSet<()>, JoinSet<()>) {
        let mut tasks = lock(&self.inner.tasks);
        tasks.closed = true;
        let sets = (
            std::mem::take(&mut tasks.active),
            std::mem::take(&mut tasks.blocking),
            std::mem::take(&mut tasks.children),
        );
        drop(tasks);
        self.inner.completed.add_permits(1);
        sets
    }

    async fn join_active(
        active: &mut JoinSet<TaskCompletion>,
        cancellation_expected: bool,
    ) -> std::result::Result<(), TaskGroupError> {
        while let Some(result) = active.join_next().await {
            match result {
                Ok(_) => {}
                Err(error) if error.is_panic() => {
                    return Err(TaskGroupError::CommandPanicked);
                }
                Err(error) if cancellation_expected && error.is_cancelled() => {}
                Err(_) => return Err(TaskGroupError::CleanupFailed),
            }
        }
        Ok(())
    }

    async fn join_blocking(blocking: &mut JoinSet<()>) -> std::result::Result<(), TaskGroupError> {
        while let Some(result) = blocking.join_next().await {
            if let Err(error) = result {
                return if error.is_panic() {
                    Err(TaskGroupError::BlockingChildPanicked)
                } else {
                    Err(TaskGroupError::CleanupFailed)
                };
            }
        }
        Ok(())
    }

    async fn join_children(
        children: &mut JoinSet<()>,
        cancellation_expected: bool,
    ) -> std::result::Result<(), TaskGroupError> {
        while let Some(result) = children.join_next().await {
            if let Err(error) = result {
                if error.is_panic() {
                    return Err(TaskGroupError::ApplicationChildPanicked);
                }
                if !(cancellation_expected && error.is_cancelled()) {
                    return Err(TaskGroupError::CleanupFailed);
                }
            }
        }
        Ok(())
    }
}

tokio::task_local! {
    static CURRENT_COMMAND_TASK_GROUP: CommandTaskGroup;
}

/// Failure while spawning into or explicitly joining the command task group.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TaskGroupError {
    #[error("the command task group is closed")]
    Closed,
    #[error("command task group cleanup failed")]
    CleanupFailed,
    #[error("a command task panicked")]
    CommandPanicked,
    #[error("an application-owned blocking command child panicked")]
    BlockingChildPanicked,
    #[error("an application-owned command child panicked")]
    ApplicationChildPanicked,
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
    #[error("an application-owned blocking command child panicked")]
    BlockingChildPanicked,
    #[error("an application-owned command child panicked")]
    ApplicationChildPanicked,
    #[error("a command task was aborted while execution was running")]
    CommandAborted,
    #[error("the command task group was closed while the manager was running")]
    TaskGroupClosed,
    #[error("the command task group cleanup failed")]
    TaskGroupCleanupFailed,
    #[error("command task group failure: {0}")]
    TaskGroup(TaskGroupError),
}

impl From<TaskGroupError> for ExecutionError {
    fn from(error: TaskGroupError) -> Self {
        match error {
            TaskGroupError::Closed => Self::TaskGroupClosed,
            TaskGroupError::CleanupFailed => Self::TaskGroupCleanupFailed,
            TaskGroupError::CommandPanicked => Self::CommandPanicked,
            TaskGroupError::BlockingChildPanicked => Self::BlockingChildPanicked,
            TaskGroupError::ApplicationChildPanicked => Self::ApplicationChildPanicked,
        }
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
    use std::sync::atomic::{AtomicU64, Ordering};
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

    struct CaptureOutput {
        id: u64,
        sender: mpsc::UnboundedSender<String>,
        flush_gate: Option<Arc<Semaphore>>,
    }

    #[derive(Debug)]
    struct CaptureTransport {
        output: mpsc::UnboundedSender<String>,
        flush_gate: Option<Arc<Semaphore>>,
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
        flush_gate: Option<Arc<Semaphore>>,
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
            if self
                .flush_gate
                .as_ref()
                .is_some_and(|gate| gate.available_permits() == 0)
            {
                return Poll::Pending;
            }
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
            let capture = CAPTURE_OUTPUTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or(CaptureConnectError)?;
            Ok(Self {
                output: capture.sender,
                flush_gate: capture.flush_gate,
            })
        }

        fn split(self) -> (Self::Incoming, Self::Outgoing) {
            (
                futures_util::stream::pending(),
                CaptureSink {
                    output: self.output,
                    flush_gate: self.flush_gate,
                },
            )
        }
    }

    // `Transport::new` has no arguments. Keep one capture sender queued per
    // test and hold TEST_LOCK until the client has consumed it; this makes
    // parallel unit tests independent without sharing an IRC connection.
    static CAPTURE_OUTPUTS: Mutex<VecDeque<CaptureOutput>> = Mutex::new(VecDeque::new());
    static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(1);
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestSender {
        id: u64,
        sender: Arc<TestChat>,
        _output: mpsc::UnboundedReceiver<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn test_sender() -> TestSender {
        test_sender_with_flush_gate(None)
    }

    fn test_sender_with_flush_gate(flush_gate: Option<Arc<Semaphore>>) -> TestSender {
        let lock = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (output_tx, output) = mpsc::unbounded_channel();
        let id = NEXT_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
        CAPTURE_OUTPUTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(CaptureOutput {
                id,
                sender: output_tx,
                flush_gate,
            });
        let config = ClientConfig::new_simple(StaticLoginCredentials::new(
            "bot".to_owned(),
            Some("token".to_owned()),
        ));
        let (incoming, client) = TwitchIRCClient::<TestTransport, TestLogin>::new(config);
        drop(incoming);
        TestSender {
            id,
            sender: ChatSender::new(Arc::new(client)),
            _output: output,
            _lock: lock,
        }
    }

    impl Drop for TestSender {
        fn drop(&mut self) {
            CAPTURE_OUTPUTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|capture| capture.id != self.id);
        }
    }

    impl TestSender {
        async fn next_output(&mut self) -> String {
            timeout(Duration::from_secs(1), self._output.recv())
                .await
                .expect("timed out waiting for captured IRC output")
                .expect("captured IRC output sender closed")
        }
    }

    fn privmsg(login: &str, id: &str) -> PrivmsgMessage {
        let raw = format!(
            "@badge-info=;badges=;color=;display-name={login};emotes=;flags=;id=msg-1;mod=0;room-id=1;subscriber=0;tmi-sent-ts=1700000000000;user-id={id};user-type= :{login}!{login}@{login}.tmi.twitch.tv PRIVMSG #test :hello"
        );
        PrivmsgMessage::try_from(IRCMessage::parse(&raw).unwrap()).unwrap()
    }

    struct TestCommand {
        id: &'static str,
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
                id,
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
                id,
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
                id,
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
                id,
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
        fn id(&self) -> &'static str {
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
            if self.observe_cancellation {
                ctx.cancellation.check()?;
                ctx.sender
                    .reply(&ctx.privmsg, "cancelled command must not reply")
                    .await;
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
            id: "ping_trigger",
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

    struct BlockingPanicCommand;

    #[async_trait]
    impl Command<TestTransport, TestLogin> for BlockingPanicCommand {
        fn id(&self) -> &'static str {
            "ai"
        }

        fn name(&self) -> &'static str {
            "!ai"
        }

        fn execution_policy(&self) -> ExecutionClass {
            ExecutionClass::Background
        }

        async fn execute(&self, _ctx: CommandContext<TestTransport, TestLogin>) -> Result<()> {
            let group = CommandTaskGroup::current().expect("command task group");
            group
                .spawn_blocking(|| panic!("intentional blocking panic"))
                .unwrap();
            Ok(())
        }
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

    async fn next_command_completion(group: &CommandTaskGroup) -> TaskCompletion {
        match group.join_next_event().await {
            TaskGroupEvent::Command(Some(Ok(completion))) => completion,
            TaskGroupEvent::Command(Some(Err(error))) => panic!("command task failed: {error}"),
            TaskGroupEvent::Command(None) => panic!("command completion unexpectedly absent"),
            TaskGroupEvent::Blocking(Ok(())) => panic!("blocking child completed first"),
            TaskGroupEvent::Child(Ok(())) => panic!("application child completed first"),
            TaskGroupEvent::Blocking(Err(error)) | TaskGroupEvent::Child(Err(error)) => {
                panic!("application child failed: {error}")
            }
            TaskGroupEvent::Closed => panic!("task group closed while waiting for a command"),
        }
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
        let mut test_sender = test_sender();
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
        let line = loop {
            let line = test_sender.next_output().await;
            if line.contains(" PRIVMSG #test :") {
                break line;
            }
        };
        let body = line.rsplit_once(" :").expect("PRIVMSG body").1;
        assert_eq!(
            body.strip_prefix(". ").unwrap_or(body),
            CAPACITY_REJECTION_MESSAGE
        );
    }

    #[tokio::test]
    async fn manager_capacity_reply_is_time_bounded_and_exact() {
        let flush_gate = Arc::new(Semaphore::new(0));
        let test_sender = test_sender_with_flush_gate(Some(flush_gate.clone()));
        let command_gate = Arc::new(Semaphore::new(0));
        let command = TestCommand::background(
            "ai",
            "!ai",
            Arc::new(AsyncMutex::new(Vec::new())),
            Arc::new(Notify::new()),
            Some(command_gate),
        );
        let limits = ExecutionLimits {
            max_active: 1,
            max_pending: 0,
            max_active_per_user: 1,
            max_pending_per_user: 0,
            max_active_per_command: 1,
            max_pending_per_command: 0,
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

        timeout(
            Duration::from_secs(1),
            manager.admit(invocation(
                test_sender.sender.clone(),
                "ai",
                "two",
                "rejected",
            )),
        )
        .await
        .expect("capacity reply blocked the manager scheduling path")
        .unwrap();

        assert_eq!(CAPACITY_REJECTION_MESSAGE, "zu viel zu tun");
        flush_gate.add_permits(1);
        manager.task_group.abort_and_join().await.unwrap();
    }

    #[tokio::test]
    async fn per_command_pending_limit_is_independent_from_user_limit() {
        let mut test_sender = test_sender();
        let command_gate = Arc::new(Semaphore::new(0));
        let command = TestCommand::background(
            "ai",
            "!ai",
            Arc::new(AsyncMutex::new(Vec::new())),
            Arc::new(Notify::new()),
            Some(command_gate),
        );
        let limits = ExecutionLimits {
            max_active: 1,
            max_pending: 4,
            max_active_per_user: 3,
            max_pending_per_user: 3,
            max_active_per_command: 1,
            max_pending_per_command: 2,
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
        for user in ["two", "three", "four"] {
            manager
                .admit(invocation(test_sender.sender.clone(), "ai", user, "queued"))
                .await
                .unwrap();
        }
        assert_eq!(manager.pending.len(), 2);
        let line = loop {
            let line = test_sender.next_output().await;
            if line.contains(" PRIVMSG #test :") {
                break line;
            }
        };
        let body = line.rsplit_once(" :").expect("PRIVMSG body").1;
        assert_eq!(body.strip_prefix(". ").unwrap_or(body), "zu viel zu tun");
        manager.task_group.abort_and_join().await.unwrap();
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
            let completion = next_command_completion(&manager.task_group).await;
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
                let completion = next_command_completion(&manager.task_group).await;
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
    async fn identity_discrepancy_tracking_is_bounded() {
        let test_sender = test_sender();
        let (_sender, mut manager) = make_manager(
            registry(vec![TestCommand::background(
                "ai",
                "!ai",
                Arc::new(AsyncMutex::new(Vec::new())),
                Arc::new(Notify::new()),
                None,
            )]),
            ExecutionLimits::default(),
        );
        for index in 0..(MAX_TRACKED_LOGIN_IDS * 2) {
            let invocation = invocation(
                test_sender.sender.clone(),
                "ai",
                &format!("user-{index}"),
                "identity",
            );
            manager.record_identity(&invocation.context);
        }
        assert_eq!(manager.recent_login_ids.len(), MAX_TRACKED_LOGIN_IDS);
    }

    #[tokio::test]
    async fn duplicate_active_completion_panics_instead_of_saturating() {
        let test_sender = test_sender();
        let (_sender, mut manager) = make_manager(
            registry(vec![TestCommand::background(
                "ai",
                "!ai",
                Arc::new(AsyncMutex::new(Vec::new())),
                Arc::new(Notify::new()),
                None,
            )]),
            ExecutionLimits::default(),
        );
        let invocation = invocation(test_sender.sender.clone(), "ai", "user", "done");
        let command_id = invocation.command_id;
        let user_key = invocation.context.user_key.clone();
        manager.increment_active(&command_id, &user_key);
        let completion = TaskCompletion {
            command_id,
            user_key,
            result: TaskResult::Succeeded,
        };
        manager.finish(completion).unwrap();
        let duplicate = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = manager.finish(TaskCompletion {
                command_id: CommandId::new("ai").unwrap(),
                user_key: UserKey::TwitchId("id-user".to_owned()),
                result: TaskResult::Succeeded,
            });
        }));
        assert!(duplicate.is_err());
    }

    #[tokio::test]
    async fn mismatched_completion_identity_panics_instead_of_saturating() {
        let (_sender, mut manager) = make_manager(
            registry(vec![TestCommand::background(
                "ai",
                "!ai",
                Arc::new(AsyncMutex::new(Vec::new())),
                Arc::new(Notify::new()),
                None,
            )]),
            ExecutionLimits::default(),
        );
        let id = CommandId::new("ai").unwrap();
        manager.increment_active(&id, &UserKey::TwitchId("id-actual".to_owned()));
        let mismatch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = manager.finish(TaskCompletion {
                command_id: id,
                user_key: UserKey::TwitchId("id-other".to_owned()),
                result: TaskResult::Succeeded,
            });
        }));
        assert!(mismatch.is_err());
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
                let completion = next_command_completion(&manager.task_group).await;
                manager.finish(completion).unwrap();
            })
            .await
            .unwrap();
            assert_eq!(manager.active, 0);
            assert!(manager.users.is_empty());
        }

        let mut test_sender = test_sender();
        let started = Arc::new(AsyncMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let command = TestCommand::cancelling("ai", "!ai", started, notify);
        let (_sender, mut manager) =
            make_manager(registry(vec![command]), ExecutionLimits::default());
        let invocation = invocation(test_sender.sender.clone(), "ai", "user", "cancelled");
        invocation.context.cancellation.cancel();
        manager.admit(invocation).await.unwrap();
        let completion = next_command_completion(&manager.task_group).await;
        manager.finish(completion).unwrap();
        assert_eq!(manager.active, 0);
        assert!(manager.users.is_empty());
        assert!(
            test_sender._output.try_recv().is_err(),
            "cancelled command crossed its reply boundary"
        );
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
        let completion = next_command_completion(&manager.task_group).await;
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
    async fn invariant_failure_uses_single_cleanup_path() {
        let test_sender = test_sender();
        let command_gate = Arc::new(Semaphore::new(0));
        let command = TestCommand::background(
            "ai",
            "!ai",
            Arc::new(AsyncMutex::new(Vec::new())),
            Arc::new(Notify::new()),
            Some(command_gate),
        );
        let (sender, manager) = make_manager(registry(vec![command]), ExecutionLimits::default());
        let group = manager.task_group();
        sender
            .sender
            .try_send(invocation(
                test_sender.sender.clone(),
                "ai",
                "active-user",
                "active",
            ))
            .unwrap();
        sender
            .sender
            .try_send(invocation(
                test_sender.sender.clone(),
                "missing",
                "bad-user",
                "unknown",
            ))
            .unwrap();
        drop(sender);

        let error = manager.run(CommandCancellation::new()).await.unwrap_err();
        assert!(error.to_string().contains("unknown command ID"));
        assert!(group.is_closed());
        assert!(group.is_empty());
    }

    #[tokio::test]
    async fn manager_escalates_blocking_panic_while_running() {
        let test_sender = test_sender();
        let (sender, manager) = make_manager(
            registry(vec![Arc::new(BlockingPanicCommand)]),
            ExecutionLimits::default(),
        );
        sender
            .sender
            .try_send(invocation(
                test_sender.sender.clone(),
                "ai",
                "user",
                "blocking-panic",
            ))
            .unwrap();

        let error = manager.run(CommandCancellation::new()).await.unwrap_err();
        assert!(
            error
                .downcast_ref::<ExecutionError>()
                .is_some_and(|error| *error == ExecutionError::BlockingChildPanicked),
            "unexpected error: {error:?}"
        );
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
    async fn completed_blocking_child_is_reaped_without_an_empty_event_hot_loop() {
        let group = CommandTaskGroup::new();
        let receiver = group.spawn_blocking(|| 7_u8).unwrap();
        assert_eq!(receiver.await.unwrap(), 7);
        assert_eq!(group.blocking_len(), 1);
        assert!(matches!(
            timeout(Duration::from_secs(1), group.join_next_event())
                .await
                .expect("manager completion event timed out"),
            TaskGroupEvent::Blocking(Ok(()))
        ));
        assert_eq!(group.blocking_len(), 0);
        assert!(group.is_empty());
    }

    #[tokio::test]
    async fn abort_and_join_closes_group_and_rejects_new_work() {
        let test_sender = test_sender();
        let group = CommandTaskGroup::new();
        group.abort_and_join().await.unwrap();
        assert!(group.is_closed());
        assert_eq!(
            group.spawn_blocking(|| 1_u8).unwrap_err(),
            TaskGroupError::Closed
        );
        assert_eq!(
            group.spawn_child(async { 1_u8 }).unwrap_err(),
            TaskGroupError::Closed
        );

        let command = TestCommand::background(
            "ai",
            "!ai",
            Arc::new(AsyncMutex::new(Vec::new())),
            Arc::new(Notify::new()),
            None,
        );
        assert_eq!(
            group.spawn_command(
                command,
                invocation(test_sender.sender.clone(), "ai", "user", "late"),
            ),
            Err(TaskGroupError::Closed)
        );
    }

    #[tokio::test]
    async fn task_group_distinguishes_blocking_and_application_child_panics() {
        let blocking = CommandTaskGroup::new();
        let receiver = blocking
            .spawn_blocking(|| panic!("intentional blocking panic"))
            .unwrap();
        assert!(receiver.await.is_err());
        assert!(matches!(
            timeout(Duration::from_secs(1), blocking.join_next_event())
                .await
                .unwrap(),
            TaskGroupEvent::Blocking(Err(ref error)) if error.is_panic()
        ));
        blocking.join_all().await.unwrap();
        assert_ne!(
            ExecutionError::from(TaskGroupError::BlockingChildPanicked),
            ExecutionError::from(TaskGroupError::ApplicationChildPanicked)
        );

        let child = CommandTaskGroup::new();
        let receiver = child
            .spawn_child(async { panic!("intentional child panic") })
            .unwrap();
        assert!(receiver.await.is_err());
        assert!(matches!(
            timeout(Duration::from_secs(1), child.join_next_event())
                .await
                .unwrap(),
            TaskGroupEvent::Child(Err(ref error)) if error.is_panic()
        ));
        child.join_all().await.unwrap();
        assert_eq!(
            ExecutionError::from(TaskGroupError::ApplicationChildPanicked),
            ExecutionError::ApplicationChildPanicked
        );
    }

    #[tokio::test]
    async fn task_group_joins_blocking_children_and_aborts_commands() {
        let group = CommandTaskGroup::new();
        let receiver = group.spawn_blocking(|| 7_u8).unwrap();
        assert_eq!(receiver.await.unwrap(), 7);
        group.join_all().await.unwrap();
        assert!(group.is_empty());

        let started = Arc::new(AsyncMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let gate = Arc::new(Semaphore::new(0));
        let test_sender = test_sender();
        let command = TestCommand::background("ai", "!ai", started, notify.clone(), Some(gate));
        let group = CommandTaskGroup::new();
        group
            .spawn_command(
                command,
                invocation(test_sender.sender.clone(), "ai", "user", "blocked"),
            )
            .unwrap();
        wait_started(&notify).await;
        group.abort_and_join().await.unwrap();
        assert!(group.is_empty());
    }
}
