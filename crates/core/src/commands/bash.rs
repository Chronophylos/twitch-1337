//! `!bash <script>`: run a throwaway script in an in-process Bashkit sandbox.
//!
//! Every invocation builds a fresh interpreter over its own in-memory VFS and
//! executes exactly one `Bash::exec`. Nothing survives it: no shell state, no
//! files, no host environment, no host process, no network. What reaches chat
//! is stdout only — stderr is captured and dropped, because a viewer-facing
//! command must not leak parser/builtin diagnostics into the channel.
//!
//! The command owns its admission path instead of relying on the generic
//! execution manager: one interpreter runs at a time by default, a second
//! concurrent run for the same user is rejected outright, and only an attempt
//! that actually reaches the interpreter is charged the per-user cooldown. No
//! wait of any kind happens before rejection, so a hostile viewer can never
//! build a queue of pending scripts.

use std::{collections::HashSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use bashkit::{
    Bash, Error as BashkitError, ExecResult, ExecutionProfile, ExecutionProfileName, FsLimits,
    LimitExceeded,
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
/// own input check, which maps onto the resource-limit reply.
const MAX_INPUT_BYTES: usize = 4096;
/// Stderr is never shown; the cap only bounds what the sandbox buffers.
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

/// Public `!bash`. Holds only admission state and a settings handle; the
/// interpreter itself is rebuilt per invocation so no script can observe
/// another script's leftovers.
pub struct BashCommand {
    settings: SettingsHandle,
    cooldown: PerUserCooldown,
    semaphore: Arc<tokio::sync::Semaphore>,
    /// Users currently holding an interpreter. Rejects a second concurrent
    /// run before it can touch the global permit queue.
    active_users: Arc<Mutex<HashSet<Arc<UserKey>>>>,
}

impl BashCommand {
    pub fn new(settings: SettingsHandle) -> Self {
        let max_concurrent = settings.load().bash.max_concurrent;
        Self {
            cooldown: PerUserCooldown::live(settings.clone(), bash_cooldown_duration),
            semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
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
fn build_interpreter(settings: &crate::settings::BashSettings) -> Bash {
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
    let fs_limits = FsLimits::new()
        .max_total_bytes(vfs_bytes)
        .max_file_size(vfs_bytes)
        .max_file_count(MAX_VFS_ENTRIES)
        .max_dir_count(MAX_VFS_ENTRIES);

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
    ctx.privmsg
        .message_text
        .strip_prefix(ctx.trigger.as_str())
        .map_or("", str::trim_start)
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
    match error {
        BashkitError::Parse { .. } => {
            Failure::Reply(format!("Syntaxfehler {}", one_of(&SYNTAX_EMOTES)))
        }
        BashkitError::ResourceLimit(
            LimitExceeded::Timeout(_) | LimitExceeded::ParserTimeout(_),
        ) => Failure::Reply(format!("Zeitlimit erreicht {}", one_of(&TIMEOUT_EMOTES))),
        BashkitError::ResourceLimit(_) => Failure::Reply(format!(
            "Ressourcenlimit erreicht {}",
            one_of(&RESOURCE_EMOTES)
        )),
        BashkitError::Cancelled => Failure::Cancelled,
        _ => Failure::Unexpected,
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

        // Admission is non-blocking at every step: a full global slot, a
        // second run from the same user, and an active cooldown all answer
        // immediately. Only a run that reaches the interpreter is charged.
        let Ok(permit) = self.semaphore.clone().try_acquire_owned() else {
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
