//! Feedback Inbox background task: classifies new entries, sends the
//! promote/attach/close notices (2 s apart), polls GitHub for closed issues,
//! and prunes old dismissed entries.
//!
//! Every external await races lifecycle cancellation (ADR-0015). A failed
//! close poll is never a Fault: one `warn` per outage, `info` on recovery.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use tokio::time::Instant;
use tracing::{debug, info};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use crate::feedback::{EntryState, FeedbackHub, HubCommand, prepare_classifier, run_classifier};
use crate::lifecycle::Lifecycle;
use crate::llm_factory::LlmRegistry;
use crate::settings::SettingsHandle;
use crate::twitch::ChatSender;

/// Gap between two feedback notices in chat.
const NOTICE_GAP: Duration = Duration::from_secs(2);
/// Dismissed-entry prune cadence; retention itself is 30 days.
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Paced chat output for feedback notices.
struct Outbox<T: Transport, L: LoginCredentials> {
    sender: Arc<ChatSender<T, L>>,
    channel: String,
    last: Option<Instant>,
}

impl<T: Transport, L: LoginCredentials> Outbox<T, L> {
    /// Send `line` at least [`NOTICE_GAP`] after the previous one. A failed
    /// send is logged by the sender; the notice counts as delivered either
    /// way. Returns `false` when cancelled.
    async fn send(&mut self, line: String, lifecycle: &Lifecycle) -> bool {
        if let Some(last) = self.last {
            tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => return false,
                () = tokio::time::sleep_until(last + NOTICE_GAP) => {}
            }
        }
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return false,
            () = self.sender.say(self.channel.clone(), line) => {}
        }
        self.last = Some(Instant::now());
        debug!("Feedback notice sent");
        true
    }
}

enum Wake {
    Command(HubCommand),
    Tick,
}

pub async fn run_feedback_handler<T, L>(
    sender: Arc<ChatSender<T, L>>,
    channel: String,
    settings: SettingsHandle,
    hub: Arc<FeedbackHub>,
    registry: Arc<LlmRegistry>,
    lifecycle: Lifecycle,
) where
    T: Transport,
    L: LoginCredentials,
{
    let Some(mut commands) = hub.take_commands() else {
        tracing::error!("Feedback handler queue was already taken; handler exiting");
        return;
    };
    let api = match reqwest::Client::builder()
        .user_agent(crate::APP_USER_AGENT)
        .build()
    {
        Ok(api) => api,
        Err(error) => {
            tracing::error!(
                ?error,
                "Failed to build feedback classifier HTTP client; handler exiting"
            );
            return;
        }
    };
    let mut out = Outbox {
        sender,
        channel,
        last: None,
    };
    let mut changes = settings.subscribe();
    let mut last_poll: Option<Instant> = None;
    let mut next_prune = Instant::now() + PRUNE_EVERY;
    let mut outage = false;
    info!(github = hub.github_configured(), "Feedback handler started");

    loop {
        let poll_every = Duration::from_secs(settings.load().feedback.close_poll_secs);
        let next_poll = hub
            .github_configured()
            .then(|| last_poll.map_or_else(Instant::now, |at| at + poll_every));
        let wake_at = next_poll.map_or(next_prune, |at| at.min(next_prune));
        let wake = tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return,
            cmd = commands.recv() => match cmd {
                Some(cmd) => Wake::Command(cmd),
                // The hub owns the sender, so this only ends with the hub.
                None => return,
            },
            () = changes.next() => Wake::Tick,
            () = tokio::time::sleep_until(wake_at) => Wake::Tick,
        };
        if lifecycle.is_cancelled() {
            return;
        }

        if let Wake::Command(cmd) = wake {
            let done = match cmd {
                HubCommand::Classify(id) => {
                    classify(&hub, &settings, &registry, &api, id, &lifecycle).await
                }
                HubCommand::Notice(line) => out.send(line, &lifecycle).await,
            };
            if !done {
                return;
            }
        }

        if next_poll.is_some_and(|at| Instant::now() >= at) {
            last_poll = Some(Instant::now());
            let polled = tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => return,
                polled = hub.poll_closes(Timestamp::now()) => polled,
            };
            match polled {
                Ok(lines) => {
                    if outage {
                        info!("Feedback close poll recovered");
                        outage = false;
                    }
                    for line in lines {
                        if !out.send(line, &lifecycle).await {
                            return;
                        }
                    }
                }
                Err(error) if !outage => {
                    crate::log_handled!(warn, error, "Feedback close poll failed");
                    outage = true;
                }
                Err(error) => debug!(?error, "Feedback close poll still failing"),
            }
        }

        if Instant::now() >= next_prune {
            next_prune = Instant::now() + PRUNE_EVERY;
            tokio::select! {
                biased;
                _ = lifecycle.wait_for_cancellation() => return,
                () = hub.prune(Timestamp::now()) => {}
            }
        }
    }
}

/// Classify one New entry with `feedback.profile`. Returns `false` when
/// cancelled. A missing profile or a failed call leaves it unclassified.
async fn classify(
    hub: &FeedbackHub,
    settings: &SettingsHandle,
    registry: &LlmRegistry,
    api: &reqwest::Client,
    id: crate::feedback::EntryId,
    lifecycle: &Lifecycle,
) -> bool {
    let Some(entry) = hub.entry(id) else {
        return true;
    };
    if entry.state != EntryState::New {
        return true;
    }
    let snapshot = settings.load();
    let Some(prepared) = prepare_classifier(registry, &snapshot) else {
        return true;
    };
    let result = tokio::select! {
        biased;
        _ = lifecycle.wait_for_cancellation() => return false,
        result = run_classifier(api, &prepared, &entry.text) => result,
    };
    if let Some(c) = result {
        tokio::select! {
            biased;
            _ = lifecycle.wait_for_cancellation() => return false,
            () = hub.apply_classification(id, c) => {}
        }
    }
    true
}
