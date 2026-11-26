//! Feedback Inbox (#775): `!fb` files a Feedback Entry, mods review it at
//! `/feedback`, and a reviewed entry becomes (or joins) a GitHub issue.
//!
//! The hub owns `$DATA_DIR/feedback.ron`. Every mutation happens under one
//! `parking_lot` mutex and is saved right after, with the lock released
//! before the write. Promote and attach run a per-entry state machine whose
//! in-flight states (`Promoting`, `Attaching`) double as the lock against
//! double clicks; an ambiguous GitHub failure parks the entry in a *Unknown
//! state that is resolved by searching for the entry's hidden marker.

mod classify;
mod inbox;
mod render;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::WrapErr as _;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::github::{GithubClient, GithubError, IssueType, MarkerScope, NewIssue};
use crate::settings::SettingsHandle;

pub use self::inbox::DISMISSED_RETENTION;
use self::inbox::Inbox;
pub use self::render::{MAX_NOTE_CHARS, MAX_TITLE_CHARS, marker, prefill_title};

pub(crate) use self::classify::{classify as run_classifier, prepare as prepare_classifier};

/// File name under `$DATA_DIR`.
pub const FILENAME: &str = "feedback.ron";

/// `system:` labels a promoted issue may carry (without the prefix).
pub const SYSTEM_LABELS: &[&str] = &[
    "ai", "aviation", "ping", "tracker", "schedule", "irc", "config", "web",
];

/// Labels every promoted issue gets.
const FIXED_LABELS: [&str; 2] = ["status:needs-triage", "source:chat"];
/// Label an attach adds to the target issue.
const ATTACH_LABEL: &str = "source:chat";

/// After an ambiguous failure, a retry that finds no marker waits this long
/// before creating again: GitHub search indexes new issues with a delay.
pub const MARKER_HOLD_OFF: SignedDuration = SignedDuration::from_mins(2);

/// Queued handler work (classify requests and chat notices). Overflow only
/// drops work the dashboard can redo (Classify button) or a notice.
const COMMAND_CAPACITY: usize = 256;

pub type EntryId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackEntry {
    pub id: EntryId,
    pub user_id: String,
    pub login: String,
    pub text: String,
    pub created_at: Timestamp,
    pub classification: Option<Classification>,
    pub state: EntryState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Bug,
    Feature,
    Task,
    Noise,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Bug, Kind::Feature, Kind::Task, Kind::Noise];

    /// Classifier choice key.
    pub fn key(self) -> &'static str {
        match self {
            Kind::Bug => "bug",
            Kind::Feature => "feature",
            Kind::Task => "task",
            Kind::Noise => "noise",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.key() == key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub kind: Kind,
    pub kind_pct: u8,
    /// A [`SYSTEM_LABELS`] entry without the `system:` prefix; `None` = none.
    pub system: Option<String>,
    pub system_pct: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkVia {
    Promoted,
    Attached,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseInfo {
    pub at: Timestamp,
    pub completed: bool,
    /// The close was handled for chat: announced, or deliberately not
    /// (older than 24 h when first seen).
    pub notified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryState {
    New,
    /// `by: None` = auto-dismissed as noise by the classifier.
    Dismissed {
        at: Timestamp,
        by: Option<String>,
    },
    Promoting {
        since: Timestamp,
        by: String,
    },
    PromoteUnknown {
        failed_at: Timestamp,
        by: String,
    },
    Attaching {
        issue: u64,
        since: Timestamp,
        by: String,
    },
    AttachUnknown {
        issue: u64,
        failed_at: Timestamp,
        by: String,
    },
    Linked {
        issue: u64,
        via: LinkVia,
        by: String,
        at: Timestamp,
        close: Option<CloseInfo>,
    },
}

/// The mod's promote form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromoteForm {
    pub title: String,
    pub issue_type: IssueType,
    /// [`SYSTEM_LABELS`] entries, without the `system:` prefix.
    pub systems: Vec<String>,
    /// Trusted Markdown above the `---` rule; empty for none.
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueLink {
    pub number: u64,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    Empty,
    TooLong,
    UnknownSystem,
    Zero,
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("feedback entry not found")]
    NotFound,
    #[error("a GitHub call for this entry is in flight")]
    Busy,
    #[error("entry is not in a state that allows this action")]
    InvalidState,
    #[error("GitHub is not configured")]
    GithubUnconfigured,
    #[error("invalid {field}: {reason:?}")]
    Invalid {
        field: &'static str,
        reason: InvalidReason,
    },
    #[error(
        "previous attempt failed ambiguously less than 2 minutes ago and no marker was found yet"
    )]
    StillChecking,
    #[error("GitHub outcome unknown; entry parked until the marker search resolves it")]
    Ambiguous,
    #[error("target issue not found")]
    IssueNotFound,
    #[error("GitHub call failed")]
    Github(#[source] GithubError),
}

/// Why `!fb` could not file an entry.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("daily feedback cap reached")]
    DailyCap,
    #[error("feedback inbox could not be saved")]
    Persist(#[source] eyre::Report),
}

/// Work the feedback handler task drains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HubCommand {
    Classify(EntryId),
    Notice(String),
}

struct State {
    inbox: Inbox,
    /// Bumped on every mutation; orders concurrent saves.
    version: u64,
}

/// A serialized inbox ready to write, tagged with its mutation version.
struct Snapshot {
    version: u64,
    ron: Result<String, ron::Error>,
}

/// Validated promote form.
struct ValidPromote {
    title: String,
    issue_type: IssueType,
    labels: Vec<String>,
    note: String,
}

pub struct FeedbackHub {
    path: PathBuf,
    github: Option<Arc<GithubClient>>,
    settings: SettingsHandle,
    state: parking_lot::Mutex<State>,
    /// Version last written; serializes writes and drops stale snapshots.
    written: tokio::sync::Mutex<u64>,
    commands: mpsc::Sender<HubCommand>,
    commands_rx: parking_lot::Mutex<Option<mpsc::Receiver<HubCommand>>>,
}

impl std::fmt::Debug for FeedbackHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedbackHub")
            .field("path", &self.path)
            .field("github", &self.github.is_some())
            .finish_non_exhaustive()
    }
}

impl FeedbackHub {
    /// Loads `$DATA_DIR/feedback.ron` (missing file = empty inbox), prunes,
    /// and builds the internal command channel. A file that does not parse
    /// aborts startup rather than being overwritten by an empty inbox.
    pub async fn open(
        data_dir: &Path,
        github: Option<Arc<GithubClient>>,
        settings: SettingsHandle,
    ) -> eyre::Result<Arc<FeedbackHub>> {
        let path = data_dir.join(FILENAME);
        let mut inbox = match tokio::fs::read_to_string(&path).await {
            Ok(text) => ron::from_str::<Inbox>(&text)
                .wrap_err_with(|| format!("parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Inbox::default(),
            Err(error) => {
                return Err(error).wrap_err_with(|| format!("read {}", path.display()));
            }
        };
        let recovered = inbox.recover_in_flight();
        let pruned = inbox.prune(Timestamp::now());
        let entries = inbox.entries.len();
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let hub = Arc::new(Self {
            path,
            github,
            settings,
            state: parking_lot::Mutex::new(State { inbox, version: 0 }),
            written: tokio::sync::Mutex::new(0),
            commands: tx,
            commands_rx: parking_lot::Mutex::new(Some(rx)),
        });
        if recovered > 0 {
            warn!(
                recovered,
                "Feedback entries were mid-GitHub-call at shutdown; parked as unknown"
            );
        }
        if recovered + pruned > 0 {
            let ((), snap) = hub.mutate_ok(|_| ());
            hub.save(snap)
                .await
                .wrap_err("save feedback inbox after recovery")?;
        }
        info!(entries, pruned, "Feedback inbox loaded");
        Ok(hub)
    }

    pub fn github_configured(&self) -> bool {
        self.github.is_some()
    }

    pub fn issue_url(&self, n: u64) -> Option<String> {
        self.github.as_ref().map(|g| g.issue_url(n))
    }

    /// Snapshot, newest first.
    pub fn entries(&self) -> Vec<FeedbackEntry> {
        self.state
            .lock()
            .inbox
            .entries
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    pub fn entry(&self, id: EntryId) -> Option<FeedbackEntry> {
        self.state.lock().inbox.entry(id).cloned()
    }

    /// File a New entry for `!fb`, subject to `feedback.per_user_daily`, and
    /// queue it for the classifier.
    pub async fn submit(
        &self,
        user_id: &str,
        login: &str,
        text: &str,
    ) -> Result<EntryId, SubmitError> {
        self.submit_at(user_id, login, text, Timestamp::now()).await
    }

    async fn submit_at(
        &self,
        user_id: &str,
        login: &str,
        text: &str,
        now: Timestamp,
    ) -> Result<EntryId, SubmitError> {
        let cfg = self.settings.load();
        let (id, snap) = self
            .mutate(|inbox| inbox.submit(user_id, login, text, now, cfg.feedback.per_user_daily))?;
        if let Err(error) = self.save(snap).await {
            // Roll back so the viewer's failure reply is the truth; the
            // next successful save writes the inbox without it.
            self.mutate_ok(|inbox| inbox.entries.retain(|e| e.id != id));
            return Err(SubmitError::Persist(error));
        }
        if !cfg.feedback.profile.trim().is_empty() {
            self.enqueue(HubCommand::Classify(id));
        }
        Ok(id)
    }

    /// Seed an entry verbatim (any state, its own id). Tests only.
    #[cfg(any(test, feature = "testing"))]
    pub async fn insert_for_tests(&self, entry: FeedbackEntry) {
        let ((), snap) = self.mutate_ok(|inbox| inbox.insert(entry));
        self.save_logged(snap).await;
    }

    /// New | PromoteUnknown | AttachUnknown → Dismissed.
    pub async fn dismiss(&self, id: EntryId, by: &str) -> Result<(), ActionError> {
        let now = Timestamp::now();
        let ((), snap) = self.mutate(|inbox| {
            let e = inbox.entry_mut(id).ok_or(ActionError::NotFound)?;
            match e.state {
                EntryState::New
                | EntryState::PromoteUnknown { .. }
                | EntryState::AttachUnknown { .. } => {}
                EntryState::Promoting { .. } | EntryState::Attaching { .. } => {
                    return Err(ActionError::Busy);
                }
                EntryState::Dismissed { .. } | EntryState::Linked { .. } => {
                    return Err(ActionError::InvalidState);
                }
            }
            e.state = EntryState::Dismissed {
                at: now,
                by: Some(by.to_owned()),
            };
            Ok(())
        })?;
        self.save_logged(snap).await;
        Ok(())
    }

    /// Dismissed → New.
    pub async fn restore(&self, id: EntryId, _by: &str) -> Result<(), ActionError> {
        let ((), snap) = self.mutate(|inbox| {
            let e = inbox.entry_mut(id).ok_or(ActionError::NotFound)?;
            if !matches!(e.state, EntryState::Dismissed { .. }) {
                return Err(ActionError::InvalidState);
            }
            e.state = EntryState::New;
            Ok(())
        })?;
        self.save_logged(snap).await;
        Ok(())
    }

    /// Queue a New entry for the classifier (dashboard Classify button).
    pub fn request_classify(&self, id: EntryId) -> Result<(), ActionError> {
        match self.entry(id).map(|e| e.state) {
            None => Err(ActionError::NotFound),
            Some(EntryState::New) => self
                .commands
                .try_send(HubCommand::Classify(id))
                .map_err(|_full| ActionError::Busy),
            Some(_) => Err(ActionError::InvalidState),
        }
    }

    pub async fn promote(
        &self,
        id: EntryId,
        form: PromoteForm,
        by: &str,
    ) -> Result<IssueLink, ActionError> {
        self.promote_at(id, form, by, Timestamp::now()).await
    }

    async fn promote_at(
        &self,
        id: EntryId,
        form: PromoteForm,
        by: &str,
        now: Timestamp,
    ) -> Result<IssueLink, ActionError> {
        let form = validate_promote(form)?;
        let gh = self.github.clone().ok_or(ActionError::GithubUnconfigured)?;
        let ((prev, entry), snap) = self.mutate(|inbox| {
            let e = inbox.entry_mut(id).ok_or(ActionError::NotFound)?;
            // The *Unknown state to restore, with its failure time.
            let prev = match &e.state {
                EntryState::New => None,
                EntryState::PromoteUnknown { failed_at, .. } => Some((*failed_at, e.state.clone())),
                EntryState::Promoting { .. } | EntryState::Attaching { .. } => {
                    return Err(ActionError::Busy);
                }
                _ => return Err(ActionError::InvalidState),
            };
            e.state = EntryState::Promoting {
                since: now,
                by: by.to_owned(),
            };
            Ok((prev, e.clone()))
        })?;
        self.save_logged(snap).await;

        if let Some((failed_at, prev)) = prev {
            match gh.find_marker(&marker(id), MarkerScope::Body).await {
                Ok(Some(number)) => {
                    info!(
                        entry = id,
                        issue = number,
                        "Feedback promote resolved by marker"
                    );
                    let url = gh.issue_url(number);
                    return Ok(self
                        .finish_link(&entry, number, url, LinkVia::Promoted, by, now)
                        .await);
                }
                Ok(None) if now.duration_since(failed_at) < MARKER_HOLD_OFF => {
                    self.set_state(id, prev).await;
                    return Err(ActionError::StillChecking);
                }
                Ok(None) => {}
                Err(error) => {
                    self.set_state(id, prev).await;
                    return Err(ActionError::Github(error));
                }
            }
        }

        let issue = NewIssue {
            title: form.title,
            body: render::issue_body(&entry, by, &form.note),
            issue_type: form.issue_type,
            labels: form.labels,
        };
        match gh.create_issue(&issue).await {
            Ok(created) => {
                info!(
                    entry = id,
                    issue = created.number,
                    "Feedback promoted to issue"
                );
                Ok(self
                    .finish_link(
                        &entry,
                        created.number,
                        created.html_url,
                        LinkVia::Promoted,
                        by,
                        now,
                    )
                    .await)
            }
            Err(GithubError::Ambiguous(source)) => {
                crate::log_handled!(warn, source, entry = id, "Feedback promote outcome unknown");
                self.set_state(
                    id,
                    EntryState::PromoteUnknown {
                        failed_at: now,
                        by: by.to_owned(),
                    },
                )
                .await;
                Err(ActionError::Ambiguous)
            }
            Err(error) => {
                self.set_state(id, EntryState::New).await;
                Err(ActionError::Github(error))
            }
        }
    }

    pub async fn attach(
        &self,
        id: EntryId,
        issue: u64,
        by: &str,
    ) -> Result<IssueLink, ActionError> {
        self.attach_at(id, issue, by, Timestamp::now()).await
    }

    async fn attach_at(
        &self,
        id: EntryId,
        issue: u64,
        by: &str,
        now: Timestamp,
    ) -> Result<IssueLink, ActionError> {
        check_issue_number(issue)?;
        let gh = self.github.clone().ok_or(ActionError::GithubUnconfigured)?;
        let ((prev, entry), snap) = self.mutate(|inbox| {
            let e = inbox.entry_mut(id).ok_or(ActionError::NotFound)?;
            let prev = match &e.state {
                EntryState::New | EntryState::AttachUnknown { .. } => e.state.clone(),
                EntryState::Promoting { .. } | EntryState::Attaching { .. } => {
                    return Err(ActionError::Busy);
                }
                _ => return Err(ActionError::InvalidState),
            };
            e.state = EntryState::Attaching {
                issue,
                since: now,
                by: by.to_owned(),
            };
            Ok((prev, e.clone()))
        })?;
        self.save_logged(snap).await;

        let target = match gh.get_issue(issue).await {
            Ok(target) => target,
            Err(GithubError::NotFound) => {
                self.set_state(id, EntryState::New).await;
                return Err(ActionError::IssueNotFound);
            }
            Err(error) => {
                self.set_state(id, prev).await;
                return Err(ActionError::Github(error));
            }
        };

        if let EntryState::AttachUnknown { failed_at, .. } = &prev {
            match gh.find_marker(&marker(id), MarkerScope::Comments).await {
                Ok(Some(number)) => {
                    info!(
                        entry = id,
                        issue = number,
                        "Feedback attach resolved by marker"
                    );
                    self.label_attached(&gh, number).await;
                    let url = gh.issue_url(number);
                    return Ok(self
                        .finish_link(&entry, number, url, LinkVia::Attached, by, now)
                        .await);
                }
                Ok(None) if now.duration_since(*failed_at) < MARKER_HOLD_OFF => {
                    self.set_state(id, prev).await;
                    return Err(ActionError::StillChecking);
                }
                Ok(None) => {}
                Err(error) => {
                    self.set_state(id, prev).await;
                    return Err(ActionError::Github(error));
                }
            }
        }

        match gh.comment(issue, &render::attach_comment(&entry, by)).await {
            Ok(()) => {}
            Err(GithubError::Ambiguous(source)) => {
                crate::log_handled!(warn, source, entry = id, "Feedback attach outcome unknown");
                self.set_state(
                    id,
                    EntryState::AttachUnknown {
                        issue,
                        failed_at: now,
                        by: by.to_owned(),
                    },
                )
                .await;
                return Err(ActionError::Ambiguous);
            }
            Err(error) => {
                self.set_state(id, EntryState::New).await;
                return Err(ActionError::Github(error));
            }
        }
        self.label_attached(&gh, issue).await;
        info!(entry = id, issue, "Feedback attached to issue");
        Ok(self
            .finish_link(&entry, issue, target.html_url, LinkVia::Attached, by, now)
            .await)
    }

    /// Link to an existing issue without writing to GitHub (the fallback for
    /// *Unknown states). Only checks that the issue exists.
    pub async fn link_manual(
        &self,
        id: EntryId,
        issue: u64,
        by: &str,
    ) -> Result<IssueLink, ActionError> {
        check_issue_number(issue)?;
        let gh = self.github.clone().ok_or(ActionError::GithubUnconfigured)?;
        let before = self.entry(id).ok_or(ActionError::NotFound)?;
        linkable(&before.state)?;
        let target = match gh.get_issue(issue).await {
            Ok(target) => target,
            Err(GithubError::NotFound) => return Err(ActionError::IssueNotFound),
            Err(error) => return Err(ActionError::Github(error)),
        };
        let now = Timestamp::now();
        let ((), snap) = self.mutate(|inbox| {
            let e = inbox.entry_mut(id).ok_or(ActionError::NotFound)?;
            // Something else acted while the lookup ran.
            if e.state != before.state {
                return Err(ActionError::Busy);
            }
            e.state = EntryState::Linked {
                issue,
                via: LinkVia::Manual,
                by: by.to_owned(),
                at: now,
                close: None,
            };
            Ok(())
        })?;
        self.save_logged(snap).await;
        info!(entry = id, issue, "Feedback linked to issue manually");
        Ok(IssueLink {
            number: issue,
            url: target.html_url,
        })
    }

    /// Take the handler's command receiver; `None` once taken.
    pub(crate) fn take_commands(&self) -> Option<mpsc::Receiver<HubCommand>> {
        self.commands_rx.lock().take()
    }

    /// Store a classifier result; auto-dismisses confident noise.
    pub(crate) async fn apply_classification(&self, id: EntryId, c: Classification) {
        let floor = self.settings.load().feedback.noise_floor;
        let now = Timestamp::now();
        match self.mutate(|inbox| inbox.classify(id, c, floor, now).ok_or(())) {
            Ok((dismissed, snap)) => {
                if dismissed {
                    debug!(entry = id, "Feedback entry auto-dismissed as noise");
                }
                self.save_logged(snap).await;
            }
            Err(()) => debug!(entry = id, "Classified feedback entry is gone"),
        }
    }

    /// Delete Dismissed entries past retention.
    pub(crate) async fn prune(&self, now: Timestamp) {
        if let Ok((pruned, snap)) = self.mutate(|inbox| match inbox.prune(now) {
            0 => Err(()),
            n => Ok(n),
        }) {
            info!(pruned, "Pruned dismissed feedback entries");
            self.save_logged(snap).await;
        }
    }

    /// One close poll. The first poll only sets the cursor. Returns the chat
    /// lines to announce (empty when `feedback.announce_close` is off).
    pub(crate) async fn poll_closes(&self, now: Timestamp) -> Result<Vec<String>, GithubError> {
        let Some(gh) = self.github.clone() else {
            return Ok(Vec::new());
        };
        let (cursor, watched) = {
            let state = self.state.lock();
            (state.inbox.cursor, state.inbox.watched_issues())
        };
        let Some(cursor) = cursor else {
            let ((), snap) = self.mutate_ok(|inbox| inbox.cursor = Some(now));
            self.save_logged(snap).await;
            return Ok(Vec::new());
        };
        let closed = gh.closed_since(cursor).await?;
        let mut duplicates = HashMap::new();
        for c in &closed {
            if c.reason == crate::github::CloseReason::Duplicate
                && watched.contains(&c.number)
                && let Some(canonical) = gh.duplicate_of(c.number).await?
            {
                duplicates.insert(c.number, canonical);
            }
        }
        let (announcements, snap) =
            self.mutate_ok(|inbox| inbox.apply_closes(&closed, &duplicates, now));
        self.save_logged(snap).await;
        if !self.settings.load().feedback.announce_close {
            return Ok(Vec::new());
        }
        Ok(announcements
            .iter()
            .flat_map(|a| render::close_lines(&a.logins, a.issue, &a.url))
            .collect())
    }

    async fn label_attached(&self, gh: &GithubClient, issue: u64) {
        if let Err(error) = gh.add_labels(issue, &[ATTACH_LABEL]).await {
            crate::log_handled!(warn, error, issue, "Failed to label attached issue");
        }
    }

    /// Linked state + save + (optional) chat notice.
    async fn finish_link(
        &self,
        entry: &FeedbackEntry,
        number: u64,
        url: String,
        via: LinkVia,
        by: &str,
        now: Timestamp,
    ) -> IssueLink {
        self.set_state(
            entry.id,
            EntryState::Linked {
                issue: number,
                via,
                by: by.to_owned(),
                at: now,
                close: None,
            },
        )
        .await;
        if self.settings.load().feedback.announce_promote {
            let line = match via {
                LinkVia::Promoted => render::promote_notice(&entry.login, entry.id, &url),
                LinkVia::Attached | LinkVia::Manual => {
                    render::attach_notice(&entry.login, entry.id, number, &url)
                }
            };
            self.enqueue(HubCommand::Notice(line));
        }
        IssueLink { number, url }
    }

    async fn set_state(&self, id: EntryId, state: EntryState) {
        match self.mutate(|inbox| {
            let e = inbox.entry_mut(id).ok_or(())?;
            e.state = state;
            Ok(())
        }) {
            Ok(((), snap)) => self.save_logged(snap).await,
            Err(()) => warn!(entry = id, "Feedback entry vanished mid-action"),
        }
    }

    fn enqueue(&self, cmd: HubCommand) {
        if let Err(error) = self.commands.try_send(cmd) {
            warn!(?error, "Feedback handler queue full; dropping work");
        }
    }

    /// Run `f` under the lock; on `Ok` bump the version and serialize, so
    /// the caller can write without holding the lock.
    fn mutate<R, E>(&self, f: impl FnOnce(&mut Inbox) -> Result<R, E>) -> Result<(R, Snapshot), E> {
        let mut state = self.state.lock();
        let out = f(&mut state.inbox)?;
        state.version += 1;
        let snap = Snapshot {
            version: state.version,
            ron: ron::ser::to_string_pretty(&state.inbox, ron::ser::PrettyConfig::default()),
        };
        Ok((out, snap))
    }

    /// [`Self::mutate`] for a mutation that cannot be refused.
    fn mutate_ok<R>(&self, f: impl FnOnce(&mut Inbox) -> R) -> (R, Snapshot) {
        match self.mutate(|inbox| Ok::<R, std::convert::Infallible>(f(inbox))) {
            Ok(done) => done,
            Err(never) => match never {},
        }
    }

    /// Atomic write; a snapshot older than the last written one is skipped.
    async fn save(&self, snap: Snapshot) -> eyre::Result<()> {
        let text = snap.ron.wrap_err("serialize feedback inbox")?;
        let mut written = self.written.lock().await;
        if *written >= snap.version {
            return Ok(());
        }
        crate::util::persist::atomic_write_bytes_async(text.as_bytes(), &self.path, FILENAME)
            .await
            .wrap_err("write feedback.ron")?;
        *written = snap.version;
        Ok(())
    }

    /// [`Self::save`] for actions whose effect already happened: the
    /// in-memory state stays authoritative and the next save retries.
    async fn save_logged(&self, snap: Snapshot) {
        if let Err(error) = self.save(snap).await {
            crate::log_handled!(error, error, "Failed to save feedback inbox");
        }
    }
}

fn check_issue_number(issue: u64) -> Result<(), ActionError> {
    if issue == 0 {
        return Err(ActionError::Invalid {
            field: "issue",
            reason: InvalidReason::Zero,
        });
    }
    Ok(())
}

fn linkable(state: &EntryState) -> Result<(), ActionError> {
    match state {
        EntryState::New | EntryState::PromoteUnknown { .. } | EntryState::AttachUnknown { .. } => {
            Ok(())
        }
        EntryState::Promoting { .. } | EntryState::Attaching { .. } => Err(ActionError::Busy),
        EntryState::Dismissed { .. } | EntryState::Linked { .. } => Err(ActionError::InvalidState),
    }
}

fn validate_promote(form: PromoteForm) -> Result<ValidPromote, ActionError> {
    let title = render::clean_title(&form.title).map_err(|reason| ActionError::Invalid {
        field: "title",
        reason,
    })?;
    let note = render::clean_note(&form.note).map_err(|reason| ActionError::Invalid {
        field: "note",
        reason,
    })?;
    let mut labels: Vec<String> = FIXED_LABELS.iter().map(|l| (*l).to_owned()).collect();
    for system in &form.systems {
        if !SYSTEM_LABELS.contains(&system.as_str()) {
            return Err(ActionError::Invalid {
                field: "systems",
                reason: InvalidReason::UnknownSystem,
            });
        }
        let label = format!("system:{system}");
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    Ok(ValidPromote {
        title,
        issue_type: form.issue_type,
        labels,
        note,
    })
}

#[cfg(test)]
mod tests;
