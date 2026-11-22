//! `!bash <script>`: run a throwaway script in an in-process Bashkit sandbox.
//!
//! Every invocation builds a fresh interpreter over its own in-memory VFS and
//! executes exactly one `Bash::exec`. Nothing survives it: no shell state, no
//! files, no host environment, no host process, no network. What reaches chat
//! is stdout only — stderr is captured and dropped, because a viewer-facing
//! command must not leak parser/builtin diagnostics into the channel.
//!
//! The command owns its admission path instead of relying on the generic
//! execution manager: the interpreter permits are shared with Aurora's
//! `run_bash` tool (`Services::bash_permits`), a second concurrent run for the
//! same user is rejected outright, and only an attempt that actually reaches
//! the interpreter is charged the per-user cooldown. No wait of any kind
//! happens before rejection, so a hostile viewer can never build a queue of
//! pending scripts.

use std::{collections::HashSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use bashkit::{
    Bash, Error as BashkitError, ExecResult, ExecutionProfile, ExecutionProfileName, LimitExceeded,
};
use eyre::Result;
use parking_lot::Mutex;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{
    Command, CommandContext, ExecutionClass, bash_cooldown_duration,
    execution::{CommandCancelled, UserKey},
};
use crate::{
    cooldown::{PerUserCooldown, format_cooldown_remaining},
    settings::SettingsHandle,
    twitch::handlers::tracker_1337::one_of,
};

/// Ingress buffer for one script. Anything longer is rejected by Bashkit's
/// own input check, which maps onto the resource-limit reply. `run_bash`
/// refuses a longer script before it builds an interpreter.
pub(crate) const MAX_INPUT_BYTES: usize = 4096;
/// Stderr the sandbox buffers. `!bash` drops it; `run_bash` hands the model
/// a shorter prefix.
const MAX_STDERR_BYTES: usize = 4096;
/// Background jobs per interpreter. Chat scripts have no use for more.
const MAX_BACKGROUND_JOBS: usize = 4;
/// Files and directories the throwaway VFS may create.
const MAX_VFS_ENTRIES: u64 = 128;

/// Virtual identity. Deliberately unserious, deliberately not the host's.
const SANDBOX_USER: &str = "unpaid-intern";
const SANDBOX_HOST: &str = "bureau-of-unsupervised-processes";
const SANDBOX_HOME: &str = "/home/unpaid-intern";

const USAGE: &str = "Nutzung: !bash <script>";
const BUSY: &str = "zu viel zu tun";

const SYNTAX_EMOTES: [&str; 3] = ["4Head", "Keepo", "LUL"];
const TIMEOUT_EMOTES: [&str; 3] = ["Waiting", "FeelsLateMan", "modCheck"];
const RESOURCE_EMOTES: [&str; 3] = ["Bonk", "NOPE", "WICKED"];
const NONZERO_EXIT_EMOTES: [&str; 3] = ["FDM", "NotLikeThis", "monkaS"];

/// The one interpreter semaphore every Bashkit run takes a permit from:
/// `!bash` and Aurora's `run_bash` tool alike. Sized once from
/// `bash.max_concurrent` (a restart knob); built at composition and carried
/// on `Services::bash_permits`.
pub fn shared_permits(settings: &SettingsHandle) -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(
        settings.load().bash.max_concurrent,
    ))
}

/// Public `!bash`. Holds only admission state and a settings handle; the
/// interpreter itself is rebuilt per invocation so no script can observe
/// another script's leftovers.
pub struct BashCommand {
    settings: SettingsHandle,
    cooldown: PerUserCooldown,
    /// [`shared_permits`]: an in-flight `run_bash` occupies the same slot.
    permits: Arc<tokio::sync::Semaphore>,
    /// Users currently holding an interpreter. Rejects a second concurrent
    /// run before it can touch the global permit queue.
    active_users: Arc<Mutex<HashSet<Arc<UserKey>>>>,
}

impl BashCommand {
    pub fn new(settings: SettingsHandle, permits: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            cooldown: PerUserCooldown::live(settings.clone(), bash_cooldown_duration),
            permits,
            active_users: Arc::new(Mutex::new(HashSet::new())),
            settings,
        }
    }

    /// Reserve the per-user slot, or `None` if this user already runs one.
    fn claim_user(&self, key: &UserKey) -> Option<ActiveUser> {
        let key = Arc::new(key.clone());
        let mut users = self.active_users.lock();
        if !users.insert(Arc::clone(&key)) {
            return None;
        }
        Some(ActiveUser {
            users: Arc::clone(&self.active_users),
            key,
        })
    }
}

/// Removes the invoking user from the active set when the invocation ends,
/// including on the cancellation and error paths.
struct ActiveUser {
    users: Arc<Mutex<HashSet<Arc<UserKey>>>>,
    key: Arc<UserKey>,
}

impl Drop for ActiveUser {
    fn drop(&mut self) {
        self.users.lock().remove(&self.key);
    }
}

/// Build one hardened, single-use interpreter from the current settings.
/// Shared by `!bash` and `run_bash`, so the sandbox cannot drift between
/// the two.
pub(crate) fn build_interpreter(settings: &crate::settings::BashSettings) -> Bash {
    let profile = ExecutionProfile::named(ExecutionProfileName::Hardened);
    let mut limits = profile.execution_limits().clone();
    let timeout = Duration::from_millis(settings.timeout_ms);
    limits.timeout = timeout;
    limits.parser_timeout = timeout;
    limits.max_input_bytes = MAX_INPUT_BYTES;
    limits.max_stdout_bytes = settings.max_stdout_bytes;
    limits.max_stderr_bytes = MAX_STDERR_BYTES;
    limits.max_background_jobs = MAX_BACKGROUND_JOBS;

    let vfs_bytes = u64::try_from(settings.max_vfs_bytes).unwrap_or(u64::MAX);
    // Start from the hardened profile, not `FsLimits::new()`: the builder
    // replaces the whole struct, and the generic defaults would relax the
    // hardened path depth (64) and path length (2048) ceilings.
    let mut fs_limits = profile.filesystem_limits().clone();
    fs_limits.max_total_bytes = vfs_bytes;
    fs_limits.max_file_size = vfs_bytes;
    fs_limits.max_file_count = MAX_VFS_ENTRIES;
    fs_limits.max_dir_count = MAX_VFS_ENTRIES;

    Bash::builder()
        .profile(profile)
        .limits(limits)
        .filesystem_limits(fs_limits)
        .username(SANDBOX_USER)
        .hostname(SANDBOX_HOST)
        .env("HOME", SANDBOX_HOME)
        .cwd(SANDBOX_HOME)
        .build()
}

/// The script exactly as typed: only the separator after the trigger is
/// trimmed, so `x='a  b'` keeps both spaces.
fn script_of<T, L>(ctx: &CommandContext<T, L>) -> &str
where
    T: Transport,
    L: LoginCredentials,
{
    // The dispatcher accepts a Twitch reply by skipping its leading
    // `@parent` mentions; the raw body still contains them, so mirror that
    // skip before slicing at the trigger.
    let text = if ctx.privmsg.reply_parent.is_some() {
        skip_leading_mentions(ctx.privmsg.message_text.as_str())
    } else {
        ctx.privmsg.message_text.as_str()
    };
    text.strip_prefix(ctx.trigger.as_str())
        .map_or("", str::trim_start)
}

/// Skip leading `@login` words, matching the dispatcher's mention rule
/// (`@` plus a non-empty run of ASCII alphanumerics/underscores).
fn skip_leading_mentions(text: &str) -> &str {
    let mut rest = text;
    while let Some(after_at) = rest.strip_prefix('@') {
        let name_len = after_at
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if name_len == 0 {
            break;
        }
        let Some(after_space) = after_at[name_len..].strip_prefix(' ') else {
            break;
        };
        rest = after_space.trim_start();
    }
    rest
}

/// One ordinary chat line for a completed run: stdout when it says anything,
/// the exit code otherwise.
fn reply_for(output: &ExecResult) -> String {
    let stdout = output.stdout.text_lossy();
    if !stdout.trim().is_empty() {
        return stdout.into_owned();
    }
    if output.exit_code == 0 {
        return "exit 0".to_owned();
    }
    format!("exit {} {}", output.exit_code, one_of(&NONZERO_EXIT_EMOTES))
}

/// Sandbox-level class of an interpreter error. `!bash` turns it into chat
/// copy, `run_bash` into a machine-readable kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    Syntax,
    Timeout,
    ResourceLimit,
    /// The run's cancellation token fired.
    Cancelled,
    /// Anything else: a Bashkit-internal failure, not the script's fault.
    Unexpected,
}

pub(crate) fn failure_kind(error: &BashkitError) -> FailureKind {
    match error {
        BashkitError::Parse { .. } => FailureKind::Syntax,
        BashkitError::ResourceLimit(
            LimitExceeded::Timeout(_) | LimitExceeded::ParserTimeout(_),
        ) => FailureKind::Timeout,
        BashkitError::ResourceLimit(_) => FailureKind::ResourceLimit,
        BashkitError::Cancelled => FailureKind::Cancelled,
        _ => FailureKind::Unexpected,
    }
}

/// What to do with an interpreter error.
enum Failure {
    /// A viewer-legible one-liner; the diagnostic stays out of chat.
    Reply(String),
    /// Lifecycle shutdown: stay silent, the manager knows this variant.
    Cancelled,
    /// Anything unexpected. Propagated so the execution manager logs it once
    /// and sends its own fallback.
    Unexpected,
}

fn classify(error: &BashkitError) -> Failure {
    match failure_kind(error) {
        FailureKind::Syntax => Failure::Reply(format!("Syntaxfehler {}", one_of(&SYNTAX_EMOTES))),
        FailureKind::Timeout => {
            Failure::Reply(format!("Zeitlimit erreicht {}", one_of(&TIMEOUT_EMOTES)))
        }
        FailureKind::ResourceLimit => Failure::Reply(format!(
            "Ressourcenlimit erreicht {}",
            one_of(&RESOURCE_EMOTES)
        )),
        FailureKind::Cancelled => Failure::Cancelled,
        FailureKind::Unexpected => Failure::Unexpected,
    }
}

#[async_trait]
impl<T, L> Command<T, L> for BashCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "bash"
    }

    fn name(&self) -> &str {
        "!bash"
    }

    fn description(&self) -> &str {
        "Führt ein Skript in einer wegwerfbaren Bash-Sandbox aus"
    }

    fn usage(&self) -> &str {
        "!bash <script>"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        ctx.cancellation.check()?;
        let script = script_of(&ctx);
        if script.is_empty() {
            ctx.sender.reply(&ctx.privmsg, USAGE).await;
            return Ok(());
        }

        // Admission is non-blocking at every step: a full global slot (held
        // by another `!bash` or by an Aurora `run_bash`), a second run from
        // the same user, and an active cooldown all answer immediately. Only
        // a run that reaches the interpreter is charged.
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            ctx.sender.reply(&ctx.privmsg, BUSY).await;
            return Ok(());
        };
        let Some(active_user) = self.claim_user(&ctx.user_key) else {
            ctx.sender.reply(&ctx.privmsg, BUSY).await;
            return Ok(());
        };
        if let Err(remaining) = self.cooldown.try_acquire(&ctx.user_key.to_string()).await {
            ctx.sender
                .reply(
                    &ctx.privmsg,
                    format!(
                        "Bitte warte noch {} Waiting",
                        format_cooldown_remaining(remaining)
                    ),
                )
                .await;
            return Ok(());
        }

        let mut bash = build_interpreter(&self.settings.load().bash);
        let result = tokio::select! {
            () = ctx.cancellation.cancelled() => {
                // The interpreter checks this token at its next command
                // boundary; the select already dropped its future.
                bash.cancellation_token()
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                drop(active_user);
                drop(permit);
                return Err(CommandCancelled.into());
            }
            result = bash.exec(script) => result,
        };
        // Released before the reply: a client that retries the moment the
        // answer arrives must hit its cooldown, not a stale busy slot.
        drop(active_user);
        drop(permit);

        match result {
            Ok(output) => {
                ctx.cancellation.check()?;
                ctx.sender.reply(&ctx.privmsg, reply_for(&output)).await;
                Ok(())
            }
            Err(error) => {
                ctx.cancellation.check()?;
                match classify(&error) {
                    Failure::Reply(reply) => {
                        ctx.sender.reply(&ctx.privmsg, reply).await;
                        Ok(())
                    }
                    Failure::Cancelled => Err(CommandCancelled.into()),
                    Failure::Unexpected => Err(error.into()),
                }
            }
        }
    }
}
