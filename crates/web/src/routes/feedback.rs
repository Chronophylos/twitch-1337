//! `/feedback` inbox (mod-gated): review `!fb` entries and turn them into
//! GitHub issues.
//!
//! Tabs (`?tab=new|linked|dismissed`) split the inbox by entry state. Every
//! entry action posts to `/feedback/{id}/<action>`. htmx gets the entry's
//! card in its new state plus the out-of-band tab strip; a rejected action
//! toasts its reason, except field errors, which re-render the card with the
//! error beside the control. Plain form posts get a flash + redirect, or the
//! full page with the error on the entry's card.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use jiff::Timestamp;
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::feedback::{
    ActionError, EntryId, EntryState, FeedbackEntry, FeedbackHub, InvalidReason, Kind, LinkVia,
    MAX_NOTE_CHARS, MAX_TITLE_CHARS, PromoteForm, SYSTEM_LABELS, prefill_title,
};
use twitch_1337_core::github::{GithubError, IssueType};
use twitch_1337_core::util::to_berlin;

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::{WebError, is_htmx};
use crate::flash;
use crate::routes::render_with;
use crate::state::WebState;

/// Days a dismissed entry stays restorable before the hub prunes it.
const DISMISSED_RETENTION_DAYS: i64 =
    twitch_1337_core::feedback::DISMISSED_RETENTION.as_hours() / 24;

const ISSUE_TYPES: [(IssueType, &str); 3] = [
    (IssueType::Bug, "Bug"),
    (IssueType::Feature, "Feature"),
    (IssueType::Task, "Task"),
];

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/feedback", get(list))
        .route("/feedback/{id}/promote", post(promote))
        .route("/feedback/{id}/attach", post(attach))
        .route("/feedback/{id}/link", post(link))
        .route("/feedback/{id}/dismiss", post(dismiss))
        .route("/feedback/{id}/restore", post(restore))
        .route("/feedback/{id}/classify", post(classify))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    New,
    Linked,
    Dismissed,
}

impl Tab {
    fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("linked") => Tab::Linked,
            Some("dismissed") => Tab::Dismissed,
            _ => Tab::New,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Tab::New => "new",
            Tab::Linked => "linked",
            Tab::Dismissed => "dismissed",
        }
    }

    /// In-flight and unknown-result entries stay in "New": they still need a
    /// moderator.
    fn of(state: &EntryState) -> Self {
        match state {
            EntryState::Linked { .. } => Tab::Linked,
            EntryState::Dismissed { .. } => Tab::Dismissed,
            EntryState::New
            | EntryState::Promoting { .. }
            | EntryState::PromoteUnknown { .. }
            | EntryState::Attaching { .. }
            | EntryState::AttachUnknown { .. } => Tab::New,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    tab: Option<String>,
}

/// Entry count per tab, for the tab strip badges.
struct TabCounts {
    new: usize,
    linked: usize,
    dismissed: usize,
}

impl TabCounts {
    fn of(entries: &[FeedbackEntry]) -> Self {
        let mut counts = TabCounts {
            new: 0,
            linked: 0,
            dismissed: 0,
        };
        for e in entries {
            match Tab::of(&e.state) {
                Tab::New => counts.new += 1,
                Tab::Linked => counts.linked += 1,
                Tab::Dismissed => counts.dismissed += 1,
            }
        }
        counts
    }
}

struct ClassView {
    kind: &'static str,
    kind_pct: u8,
    /// System label without the `system:` prefix; "none" when unassigned.
    system: String,
    system_pct: u8,
}

/// Promote / attach / link inputs: prefilled from the entry, or the values a
/// rejected submit carried.
#[derive(Debug, Default, Clone)]
struct FormState {
    title: String,
    /// "Bug" | "Feature" | "Task"; empty selects none.
    issue_type: String,
    systems: Vec<String>,
    note: String,
    attach_issue: String,
    link_issue: String,
    /// Form whose `<details>` renders open: "promote" | "attach" | "link".
    open: &'static str,
}

/// One error slot per form control; `set` drops unknown slots.
#[derive(Debug, Default, Clone)]
struct FormErrors {
    title: Option<String>,
    issue_type: Option<String>,
    systems: Option<String>,
    note: Option<String>,
    attach_issue: Option<String>,
    link_issue: Option<String>,
}

impl FormErrors {
    fn set(&mut self, slot: &str, msg: String) -> bool {
        let target = match slot {
            "title" => &mut self.title,
            "issue_type" => &mut self.issue_type,
            "systems" => &mut self.systems,
            "note" => &mut self.note,
            "attach_issue" => &mut self.attach_issue,
            "link_issue" => &mut self.link_issue,
            _ => return false,
        };
        *target = Some(msg);
        true
    }
}

struct EntryView {
    id: EntryId,
    login: String,
    text: String,
    created: String,
    classification: Option<ClassView>,
    /// "new" | "promoting" | "promote_unknown" | "attaching" |
    /// "attach_unknown" | "linked" | "dismissed"
    state: &'static str,
    /// Human status for every state but "new".
    status: Option<String>,
    issue: Option<u64>,
    issue_url: Option<String>,
    /// Linked only: "Open", "Completed", …
    close: Option<&'static str>,
    /// Dismissed only.
    days_left: Option<i64>,
    github_configured: bool,
    form: FormState,
    errors: FormErrors,
    /// Card-level outcome of the last action on this card.
    error: Option<String>,
    notice: Option<String>,
}

impl EntryView {
    /// New and unknown-result entries take promote/attach/dismiss.
    fn actionable(&self) -> bool {
        matches!(self.state, "new" | "promote_unknown" | "attach_unknown")
    }

    fn unknown(&self) -> bool {
        matches!(self.state, "promote_unknown" | "attach_unknown")
    }

    fn max_title(&self) -> usize {
        MAX_TITLE_CHARS
    }

    fn max_note(&self) -> usize {
        MAX_NOTE_CHARS
    }

    fn can_classify(&self) -> bool {
        self.state == "new" && self.classification.is_none()
    }

    /// Dismissed only: when the hub prunes the entry.
    fn deletion_label(&self) -> Option<String> {
        self.days_left.map(|d| match d {
            0 => "Deleted today".to_owned(),
            1 => "Deleted in 1 day".to_owned(),
            d => format!("Deleted in {d} days"),
        })
    }

    fn type_options(&self) -> [(&'static str, bool); 3] {
        ISSUE_TYPES.map(|(_, label)| (label, self.form.issue_type == label))
    }

    fn system_options(&self) -> Vec<(&'static str, bool)> {
        SYSTEM_LABELS
            .iter()
            .map(|s| (*s, self.form.systems.iter().any(|c| c == s)))
            .collect()
    }
}

#[derive(Template)]
#[template(path = "feedback/index.html")]
struct ListTpl {
    entries: Vec<EntryView>,
    tab: &'static str,
    counts: TabCounts,
    github_configured: bool,
    flash: Option<String>,
    /// Banner for a rejected plain form post.
    error: Option<String>,
    csrf: String,
    user_login: String,
    user_avatar_url: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
}

/// htmx answer to an entry action: the card plus the out-of-band tab strip.
#[derive(Template)]
#[template(path = "feedback/_swap.html")]
struct SwapTpl {
    card: Option<EntryView>,
    tab: &'static str,
    counts: TabCounts,
    csrf: String,
}

#[derive(Debug, Deserialize)]
struct PromoteFields {
    #[serde(default)]
    _csrf: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    issue_type: String,
    #[serde(default)]
    systems: Vec<String>,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize)]
struct IssueFields {
    #[serde(default)]
    _csrf: String,
    #[serde(default)]
    issue: String,
}

#[derive(Debug, Deserialize)]
struct CsrfOnly {
    #[serde(default)]
    _csrf: String,
}

/// A rejected action: the reason, where to show it, and the inputs to keep.
struct Failure {
    status: StatusCode,
    message: String,
    /// Form control the message belongs beside; `None` = card-level.
    slot: Option<&'static str>,
    form: FormState,
}

fn berlin(ts: Timestamp) -> String {
    to_berlin(ts).strftime("%Y-%m-%d %H:%M").to_string()
}

fn kind_label(kind: &Kind) -> &'static str {
    match kind {
        Kind::Bug => "Bug",
        Kind::Feature => "Feature",
        Kind::Task => "Task",
        Kind::Noise => "Noise",
    }
}

fn parse_issue_type(raw: &str) -> Option<IssueType> {
    ISSUE_TYPES
        .iter()
        .find(|(_, label)| *label == raw)
        .map(|(t, _)| *t)
}

/// Accepts `123` or `#123`.
fn parse_issue(raw: &str) -> Option<u64> {
    raw.trim().trim_start_matches('#').parse().ok()
}

/// Promote form prefill: title from the text, type and system from the
/// classifier (noise and unclassified → Task).
fn prefilled_form(e: &FeedbackEntry) -> FormState {
    let (issue_type, systems) = match &e.classification {
        Some(c) => (
            match c.kind {
                Kind::Bug => "Bug",
                Kind::Feature => "Feature",
                Kind::Task | Kind::Noise => "Task",
            },
            c.system
                .iter()
                .filter(|s| SYSTEM_LABELS.contains(&s.as_str()))
                .cloned()
                .collect(),
        ),
        None => ("Task", Vec::new()),
    };
    let link_issue = match &e.state {
        EntryState::AttachUnknown { issue, .. } => issue.to_string(),
        _ => String::new(),
    };
    FormState {
        title: prefill_title(&e.text),
        issue_type: issue_type.to_owned(),
        systems,
        link_issue,
        ..FormState::default()
    }
}

fn entry_view(hub: &FeedbackHub, e: &FeedbackEntry, now: Timestamp) -> EntryView {
    let mut issue = None;
    let mut close = None;
    let mut days_left = None;
    let (state, status) = match &e.state {
        EntryState::New => ("new", None),
        EntryState::Promoting { since, by } => (
            "promoting",
            Some(format!(
                "Creating the issue (started {} by {by})",
                berlin(*since)
            )),
        ),
        EntryState::PromoteUnknown { failed_at, by } => (
            "promote_unknown",
            Some(format!(
                "Promote by {by} at {} ended with an unknown result; GitHub may have created the issue. Promote again (it checks GitHub first) or link the issue.",
                berlin(*failed_at)
            )),
        ),
        EntryState::Attaching {
            issue: n,
            since,
            by,
        } => {
            issue = Some(*n);
            (
                "attaching",
                Some(format!(
                    "Attaching to #{n} (started {} by {by})",
                    berlin(*since)
                )),
            )
        }
        EntryState::AttachUnknown {
            issue: n,
            failed_at,
            by,
        } => {
            issue = Some(*n);
            (
                "attach_unknown",
                Some(format!(
                    "Attaching to #{n} by {by} at {} ended with an unknown result. Attach again (it checks GitHub first) or link it.",
                    berlin(*failed_at)
                )),
            )
        }
        EntryState::Linked {
            issue: n,
            via,
            by,
            at,
            close: info,
        } => {
            issue = Some(*n);
            close = Some(match info {
                None => "Open",
                Some(c) if c.completed && c.notified => "Completed · reporter notified",
                Some(c) if c.completed => "Completed",
                Some(_) => "Closed (not completed)",
            });
            let verb = match via {
                LinkVia::Promoted => "Promoted",
                LinkVia::Attached => "Attached",
                LinkVia::Manual => "Linked",
            };
            ("linked", Some(format!("{verb} by {by} · {}", berlin(*at))))
        }
        EntryState::Dismissed { at, by } => {
            let age_days = now.duration_since(*at).as_secs() / 86_400;
            days_left =
                Some((DISMISSED_RETENTION_DAYS - age_days).clamp(0, DISMISSED_RETENTION_DAYS));
            let who = match by {
                Some(by) => format!("Dismissed by {by}"),
                None => "Auto-dismissed as noise".to_owned(),
            };
            ("dismissed", Some(format!("{who} · {}", berlin(*at))))
        }
    };
    EntryView {
        id: e.id,
        login: e.login.clone(),
        text: e.text.clone(),
        created: berlin(e.created_at),
        classification: e.classification.as_ref().map(|c| ClassView {
            kind: kind_label(&c.kind),
            kind_pct: c.kind_pct,
            system: c.system.clone().unwrap_or_else(|| "none".to_owned()),
            system_pct: c.system_pct,
        }),
        state,
        status,
        issue,
        issue_url: issue.and_then(|n| hub.issue_url(n)),
        close,
        days_left,
        github_configured: hub.github_configured(),
        form: prefilled_form(e),
        errors: FormErrors::default(),
        error: None,
        notice: None,
    }
}

/// English reason for a rejected action. `issue` names the target issue of
/// attach/link, for `IssueNotFound`.
fn describe(err: &ActionError, issue: Option<u64>) -> (StatusCode, String) {
    let ambiguous = || {
        (
            StatusCode::GATEWAY_TIMEOUT,
            "GitHub did not answer in time, so the result is unknown. The next attempt checks GitHub first and will not create a duplicate.".to_owned(),
        )
    };
    match err {
        ActionError::NotFound => (
            StatusCode::NOT_FOUND,
            "This feedback entry no longer exists.".to_owned(),
        ),
        ActionError::Busy => (StatusCode::CONFLICT, "Already in progress.".to_owned()),
        ActionError::InvalidState => (
            StatusCode::CONFLICT,
            "This entry changed in the meantime; reload the page.".to_owned(),
        ),
        ActionError::GithubUnconfigured => (
            StatusCode::SERVICE_UNAVAILABLE,
            "GitHub is not configured.".to_owned(),
        ),
        ActionError::Invalid { field, reason } => {
            let msg = match (*field, reason) {
                ("title", InvalidReason::Empty) => "Title must not be empty.".to_owned(),
                ("title", InvalidReason::TooLong) => {
                    format!("Title must be at most {MAX_TITLE_CHARS} characters.")
                }
                ("note", InvalidReason::TooLong) => {
                    format!("Note must be at most {MAX_NOTE_CHARS} characters.")
                }
                (_, InvalidReason::Empty) => "Must not be empty.".to_owned(),
                (_, InvalidReason::TooLong) => "Too long.".to_owned(),
                (_, InvalidReason::UnknownSystem) => "Unknown system label.".to_owned(),
                (_, InvalidReason::Zero) => "Issue number must be at least 1.".to_owned(),
            };
            (StatusCode::BAD_REQUEST, msg)
        }
        ActionError::StillChecking => (
            StatusCode::CONFLICT,
            "GitHub is still indexing; try again in a moment.".to_owned(),
        ),
        ActionError::Ambiguous => ambiguous(),
        ActionError::IssueNotFound => (
            StatusCode::BAD_REQUEST,
            match issue {
                Some(n) => format!("Issue #{n} does not exist."),
                None => "That issue does not exist.".to_owned(),
            },
        ),
        ActionError::Github(GithubError::Ambiguous(..)) => ambiguous(),
        ActionError::Github(GithubError::Auth(..)) => (
            StatusCode::BAD_GATEWAY,
            "GitHub refused the bot's credentials or rate-limited it. Nothing was created; try again later.".to_owned(),
        ),
        ActionError::Github(GithubError::NotFound) => (
            StatusCode::BAD_GATEWAY,
            "GitHub could not find the repository or issue.".to_owned(),
        ),
        ActionError::Github(GithubError::Rejected { message, .. }) => (
            StatusCode::BAD_GATEWAY,
            format!("GitHub rejected the request: {message}"),
        ),
    }
}

/// Failure for an `ActionError`; field errors land in their control's slot
/// (`issue` in the attach or link form, whichever was submitted).
fn failure(err: &ActionError, issue: Option<u64>, form: FormState) -> Failure {
    let (status, message) = describe(err, issue);
    let slot = match err {
        ActionError::Invalid { field, .. } => match (*field, form.open) {
            ("title", _) => Some("title"),
            ("note", _) => Some("note"),
            ("systems", _) => Some("systems"),
            ("issue", "attach") => Some("attach_issue"),
            ("issue", "link") => Some("link_issue"),
            _ => None,
        },
        _ => None,
    };
    Failure {
        status,
        message,
        slot,
        form,
    }
}

fn field_failure(slot: &'static str, message: &str, form: FormState) -> Failure {
    Failure {
        status: StatusCode::BAD_REQUEST,
        message: message.to_owned(),
        slot: Some(slot),
        form,
    }
}

fn log_outcome(session: &Session, action: &'static str, id: EntryId, err: Option<&ActionError>) {
    match err {
        None => tracing::info!(
            target: "twitch_1337_web",
            user_id = %session.user_id,
            action,
            entry = id,
            result = "ok",
        ),
        Some(err @ (ActionError::Github(_) | ActionError::Ambiguous)) => tracing::warn!(
            target: "twitch_1337_web",
            user_id = %session.user_id,
            action,
            entry = id,
            result = "github_error",
            error = ?err,
        ),
        Some(err) => tracing::debug!(
            target: "twitch_1337_web",
            user_id = %session.user_id,
            action,
            entry = id,
            result = "rejected",
            error = ?err,
        ),
    }
}

async fn list(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Query(q): Query<ListQuery>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let tab = Tab::parse(q.tab.as_deref());
    render_page(
        &state,
        &session,
        tab,
        StatusCode::OK,
        flash::take(&cookies),
        None,
        None,
    )
}

/// Full page for `tab`. `card` replaces the matching entry's card (a
/// rejected plain post keeps its inputs and errors there).
fn render_page(
    state: &WebState,
    session: &Session,
    tab: Tab,
    status: StatusCode,
    flash: Option<String>,
    error: Option<String>,
    card: Option<EntryView>,
) -> Result<Response, WebError> {
    let hub = &state.feedback;
    let all = hub.entries();
    let now = Timestamp::now();
    let mut card = card;
    let entries = all
        .iter()
        .filter(|e| Tab::of(&e.state) == tab)
        .map(|e| match card.take_if(|c| c.id == e.id) {
            Some(c) => c,
            None => entry_view(hub, e, now),
        })
        .collect();
    render_with(
        status,
        &ListTpl {
            entries,
            tab: tab.slug(),
            counts: TabCounts::of(&all),
            github_configured: hub.github_configured(),
            flash,
            error,
            csrf: csrf::encode(&session.csrf_value),
            user_login: session.user_login.clone(),
            user_avatar_url: session.avatar_url.clone(),
            current_page: crate::nav::FEEDBACK,
            is_mod: session.is_mod(),
            is_broadcaster: session.is_broadcaster,
            is_owner: matches!(session.role, crate::auth::Role::Owner),
        },
    )
}

/// Shared tail of every entry action. `back` is the tab the action is
/// offered on (plain posts return there).
fn respond(
    state: &WebState,
    session: &Session,
    cookies: &Cookies,
    headers: &HeaderMap,
    id: EntryId,
    back: Tab,
    outcome: Result<String, Failure>,
) -> Result<Response, WebError> {
    let hub = &state.feedback;
    let htmx = is_htmx(headers);
    match outcome {
        Ok(msg) if htmx => {
            let card = hub.entry(id).map(|e| {
                let mut view = entry_view(hub, &e, Timestamp::now());
                view.notice = Some(msg);
                view
            });
            swap(state, session, back, card)
        }
        Ok(msg) => {
            flash::set(cookies, &msg);
            Ok(Redirect::to(&format!("/feedback?tab={}", back.slug())).into_response())
        }
        // htmx keeps the submitted form in place; the toast carries the reason.
        Err(f) if htmx && f.slot.is_none() => Ok((f.status, f.message).into_response()),
        Err(f) => {
            let Some(entry) = hub.entry(id) else {
                let (status, message) = describe(&ActionError::NotFound, None);
                return if htmx {
                    Ok((status, message).into_response())
                } else {
                    render_page(state, session, back, status, None, Some(message), None)
                };
            };
            let mut view = entry_view(hub, &entry, Timestamp::now());
            view.form = f.form;
            let field_error = f
                .slot
                .is_some_and(|slot| view.errors.set(slot, f.message.clone()));
            let banner = if field_error {
                format!("F{id}: fix the highlighted field.")
            } else {
                let banner = format!("F{id}: {}", f.message);
                view.error = Some(f.message);
                banner
            };
            if htmx {
                // Field errors swap in with a 200: htmx does not swap 4xx.
                swap(state, session, back, Some(view))
            } else {
                render_page(
                    state,
                    session,
                    Tab::of(&entry.state),
                    f.status,
                    None,
                    Some(banner),
                    Some(view),
                )
            }
        }
    }
}

fn swap(
    state: &WebState,
    session: &Session,
    tab: Tab,
    card: Option<EntryView>,
) -> Result<Response, WebError> {
    render_with(
        StatusCode::OK,
        &SwapTpl {
            card,
            tab: tab.slug(),
            counts: TabCounts::of(&state.feedback.entries()),
            csrf: csrf::encode(&session.csrf_value),
        },
    )
}

async fn promote(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(id): Path<EntryId>,
    cookies: Cookies,
    headers: HeaderMap,
    axum_extra::extract::Form(form): axum_extra::extract::Form<PromoteFields>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let submitted = FormState {
        title: form.title.clone(),
        issue_type: form.issue_type.clone(),
        systems: form.systems.clone(),
        note: form.note.clone(),
        open: "promote",
        ..FormState::default()
    };
    let outcome = match parse_issue_type(&form.issue_type) {
        None => Err(field_failure(
            "issue_type",
            "Choose Bug, Feature or Task.",
            submitted,
        )),
        Some(issue_type) => {
            let result = state
                .feedback
                .promote(
                    id,
                    PromoteForm {
                        title: form.title,
                        issue_type,
                        systems: form.systems,
                        note: form.note,
                    },
                    &session.user_login,
                )
                .await;
            log_outcome(&session, "feedback_promote", id, result.as_ref().err());
            result
                .map(|link| format!("F{id} is now issue #{}.", link.number))
                .map_err(|e| failure(&e, None, submitted))
        }
    };
    respond(&state, &session, &cookies, &headers, id, Tab::New, outcome)
}

async fn attach(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(id): Path<EntryId>,
    cookies: Cookies,
    headers: HeaderMap,
    axum::Form(form): axum::Form<IssueFields>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let submitted = FormState {
        attach_issue: form.issue.clone(),
        open: "attach",
        ..entry_form(&state, id)
    };
    let outcome = match parse_issue(&form.issue) {
        None => Err(field_failure(
            "attach_issue",
            "Enter an issue number, e.g. 123.",
            submitted,
        )),
        Some(issue) => {
            let result = state.feedback.attach(id, issue, &session.user_login).await;
            log_outcome(&session, "feedback_attach", id, result.as_ref().err());
            result
                .map(|link| format!("F{id} is attached to #{}.", link.number))
                .map_err(|e| failure(&e, Some(issue), submitted))
        }
    };
    respond(&state, &session, &cookies, &headers, id, Tab::New, outcome)
}

async fn link(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(id): Path<EntryId>,
    cookies: Cookies,
    headers: HeaderMap,
    axum::Form(form): axum::Form<IssueFields>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let submitted = FormState {
        link_issue: form.issue.clone(),
        open: "link",
        ..entry_form(&state, id)
    };
    let outcome = match parse_issue(&form.issue) {
        None => Err(field_failure(
            "link_issue",
            "Enter an issue number, e.g. 123.",
            submitted,
        )),
        Some(issue) => {
            let result = state
                .feedback
                .link_manual(id, issue, &session.user_login)
                .await;
            log_outcome(&session, "feedback_link", id, result.as_ref().err());
            result
                .map(|link| format!("F{id} is linked to #{}.", link.number))
                .map_err(|e| failure(&e, Some(issue), submitted))
        }
    };
    respond(&state, &session, &cookies, &headers, id, Tab::New, outcome)
}

/// The entry's prefilled form, so a rejected attach/link keeps the promote
/// form as it was rendered.
fn entry_form(state: &WebState, id: EntryId) -> FormState {
    state
        .feedback
        .entry(id)
        .map(|e| prefilled_form(&e))
        .unwrap_or_default()
}

async fn dismiss(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(id): Path<EntryId>,
    cookies: Cookies,
    headers: HeaderMap,
    axum::Form(form): axum::Form<CsrfOnly>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let result = state.feedback.dismiss(id, &session.user_login).await;
    log_outcome(&session, "feedback_dismiss", id, result.as_ref().err());
    let outcome = result
        .map(|()| format!("F{id} dismissed."))
        .map_err(|e| failure(&e, None, entry_form(&state, id)));
    respond(&state, &session, &cookies, &headers, id, Tab::New, outcome)
}

async fn restore(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(id): Path<EntryId>,
    cookies: Cookies,
    headers: HeaderMap,
    axum::Form(form): axum::Form<CsrfOnly>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let result = state.feedback.restore(id, &session.user_login).await;
    log_outcome(&session, "feedback_restore", id, result.as_ref().err());
    let outcome = result
        .map(|()| format!("F{id} restored to New."))
        .map_err(|e| failure(&e, None, entry_form(&state, id)));
    respond(
        &state,
        &session,
        &cookies,
        &headers,
        id,
        Tab::Dismissed,
        outcome,
    )
}

async fn classify(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(id): Path<EntryId>,
    cookies: Cookies,
    headers: HeaderMap,
    axum::Form(form): axum::Form<CsrfOnly>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let result = state.feedback.request_classify(id);
    log_outcome(&session, "feedback_classify", id, result.as_ref().err());
    let outcome = result
        .map(|()| format!("F{id} queued for classification."))
        .map_err(|e| failure(&e, None, entry_form(&state, id)));
    respond(&state, &session, &cookies, &headers, id, Tab::New, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_issue_accepts_hash_prefix() {
        assert_eq!(parse_issue("123"), Some(123));
        assert_eq!(parse_issue(" #42 "), Some(42));
        assert_eq!(parse_issue("abc"), None);
        assert_eq!(parse_issue(""), None);
    }

    #[test]
    fn issue_type_round_trips_labels() {
        for (t, label) in ISSUE_TYPES {
            assert_eq!(parse_issue_type(label), Some(t));
        }
        assert_eq!(parse_issue_type("Security"), None);
    }

    #[test]
    fn issue_not_found_names_the_issue() {
        let (status, msg) = describe(&ActionError::IssueNotFound, Some(7));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(msg, "Issue #7 does not exist.");
    }
}
