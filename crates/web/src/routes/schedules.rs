//! `/schedules` CRUD handlers (mod-gated).
//!
//! The list page renders a card-grid of all schedules plus an add/edit form.
//! Add: `?new=true`. Edit: `?edit=<name>`. Validation errors re-render the
//! list with per-row error attribution. A card's on/off toggle and delete
//! answer htmx with just that card (nothing after a delete) plus an
//! out-of-band stat strip; plain form posts get a flash + redirect.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use jiff::civil::{Date, Time};
use jiff::{Timestamp, ZonedDateTime};
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::schedule::{
    Schedule, ScheduleRuntime, Trigger, WeekdaySet, parse_weekday, weekday_abbr,
};
use twitch_1337_core::settings::{Actor, SettingsError};
use twitch_1337_core::util::to_berlin;

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::{WebError, is_htmx};
use crate::flash;
use crate::routes::{humanize_delta, render};
use crate::state::WebState;

#[derive(Debug, Clone)]
pub(crate) struct RowError {
    pub row_index: usize,
    pub row_name: String,
    pub field: String,
    pub message: String,
}

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/schedules", get(list))
        .route("/schedules/add", post(create))
        .route("/schedules/{name}/edit", post(update))
        .route("/schedules/{name}/delete", post(delete))
        .route("/schedules/{name}/toggle", post(toggle))
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    /// When `Some(name)`, the matching row renders in edit mode.
    edit: Option<String>,
    /// When `Some(true)`, the "new schedule" form is shown.
    new: Option<bool>,
}

struct CardView {
    name: String,
    message: String,
    ping: Option<String>,
    enabled: bool,
    trigger_label: String,
    /// Human-rendered active-window label, e.g. "09:00–17:00".
    schedule_window_label: Option<String>,
    last_fired_human: Option<String>,
    fires_today: u32,
    next_fire_human: Option<String>,
}

struct NextFire {
    name: String,
    time: String,
}

/// Heading count + stat strip.
struct StatsView {
    total: usize,
    active_count: usize,
    fired_today: u32,
    next_fire: Option<NextFire>,
}

impl StatsView {
    fn new(
        rows: &[Schedule],
        runtimes: &HashMap<String, ScheduleRuntime>,
        now_berlin: &ZonedDateTime,
    ) -> Self {
        let next_fire = rows
            .iter()
            .filter(|r| r.enabled)
            .filter_map(|r| {
                r.next_active_fire(now_berlin.clone())
                    .map(|t| (r.name.clone(), t))
            })
            .min_by_key(|(_, t)| t.timestamp())
            .map(|(name, t)| NextFire {
                name,
                time: t.strftime("%H:%M").to_string(),
            });
        Self {
            total: rows.len(),
            active_count: rows.iter().filter(|r| r.enabled).count(),
            fired_today: runtimes.values().map(|r| r.fires_today).sum(),
            next_fire,
        }
    }
}

#[derive(Default, Debug, Clone)]
struct FormState {
    name: String,
    message: String,
    /// Ping name without `!`; empty means none.
    ping: String,
    /// "interval" | "calendar"
    kind: String,
    /// Interval split for the two number inputs; blank reads as 0.
    interval_hours: String,
    interval_minutes: String,
    interval_active_from: String,
    interval_active_to: String,
    interval_days: BTreeSet<String>,
    calendar_at: String,
    calendar_days: BTreeSet<String>,
    start_date: String,
    end_date: String,
    enabled: bool,
}

#[derive(Template)]
#[template(path = "schedules/index.html")]
struct ListTpl {
    rows: Vec<CardView>,
    edit_form: Option<FormState>,
    new_form: Option<FormState>,
    row_errors: Vec<RowError>,
    global_errors: Vec<(String, String)>,
    flash: Option<String>,
    csrf: String,
    user_login: String,
    user_avatar_url: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
    stats: StatsView,
}

/// htmx answer to a card toggle or delete: the card (`None` after a delete,
/// so the `outerHTML` swap removes it) plus the out-of-band stat strip.
#[derive(Template)]
#[template(path = "schedules/_swap.html")]
struct SwapTpl {
    card: Option<CardView>,
    csrf: String,
    stats: StatsView,
}

/// Fresh [`SwapTpl`] from the persisted schedules; `card` names the card to
/// re-render, if it still exists.
async fn swap_tpl(state: &WebState, session: &Session, card: Option<&str>) -> SwapTpl {
    let rows = state.settings.load().schedules.clone();
    let now_utc = Timestamp::now();
    let now_berlin = to_berlin(now_utc);
    let runtimes = state.schedule_runtime.snapshot(now_utc).await;
    SwapTpl {
        card: card
            .and_then(|name| rows.iter().find(|s| s.name == name))
            .map(|s| card_view(s, runtimes.get(&s.name), now_utc, &now_berlin)),
        csrf: csrf::encode(&session.csrf_value),
        stats: StatsView::new(&rows, &runtimes, &now_berlin),
    }
}

#[derive(Debug, Deserialize)]
struct ScheduleForm {
    _csrf: String,
    name: String,
    message: String,
    #[serde(default)]
    ping: String,
    /// "interval" or "calendar"
    kind: String,
    // Interval fields
    #[serde(default)]
    interval_hours: String,
    #[serde(default)]
    interval_minutes: String,
    #[serde(default)]
    interval_active_from: String,
    #[serde(default)]
    interval_active_to: String,
    #[serde(default)]
    interval_days: Vec<String>, // "Mon".."Sun"
    // Calendar fields
    #[serde(default)]
    calendar_at: String,
    #[serde(default)]
    calendar_days: Vec<String>,
    // Common date range
    #[serde(default)]
    start_date: String, // YYYY-MM-DD
    #[serde(default)]
    end_date: String,
    #[serde(default)]
    enabled: Option<String>,
}

impl ScheduleForm {
    fn try_into_schedule(self) -> Result<Schedule, Vec<twitch_1337_core::settings::FieldError>> {
        let mut errs = Vec::new();
        let prefix = "form";
        let trigger = match self.kind.as_str() {
            "interval" => {
                let secs = match parse_interval(&self.interval_hours, &self.interval_minutes) {
                    Ok(secs) => secs,
                    Err((field, message)) => {
                        errs.push(twitch_1337_core::settings::FieldError {
                            field: format!("{prefix}.{field}"),
                            message,
                        });
                        0
                    }
                };
                let days = parse_weekdays(&self.interval_days);
                let active_from = parse_hhmm(&self.interval_active_from);
                let active_to = parse_hhmm(&self.interval_active_to);
                Trigger::Interval {
                    every: Duration::from_secs(secs),
                    days,
                    active_from,
                    active_to,
                }
            }
            "calendar" => {
                let at = match parse_hhmm(&self.calendar_at) {
                    Some(v) => v,
                    None => {
                        errs.push(twitch_1337_core::settings::FieldError {
                            field: format!("{prefix}.calendar_at"),
                            message: format!("must be HH:MM (got {:?})", self.calendar_at),
                        });
                        Time::MIN
                    }
                };
                let days = parse_weekdays(&self.calendar_days);
                Trigger::Calendar { days, at }
            }
            other => {
                errs.push(twitch_1337_core::settings::FieldError {
                    field: format!("{prefix}.kind"),
                    message: format!("unknown trigger kind {other:?}"),
                });
                Trigger::Calendar {
                    days: WeekdaySet::default(),
                    at: Time::MIN,
                }
            }
        };
        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(Schedule {
            name: self.name.trim().to_owned(),
            message: self.message.trim().to_owned(),
            ping: parse_ping(&self.ping),
            trigger,
            start_date: parse_date(&self.start_date),
            end_date: parse_date(&self.end_date),
            enabled: self.enabled.is_some(),
        })
    }
}

/// Accepts `name` or `!name`; blank means no ping.
fn parse_ping(s: &str) -> Option<String> {
    let s = s.trim().trim_start_matches('!');
    (!s.is_empty()).then(|| s.to_owned())
}

fn parse_hhmm(s: &str) -> Option<Time> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Time::strptime("%H:%M", s).ok()
}

/// Seconds for the interval's hours + minutes inputs. Blank reads as 0, so a
/// zero interval reaches `Schedule::validate` (which rejects it) unchanged.
/// The 24 h cap lives here, not in `Schedule::validate`: that also runs at
/// boot, where one failing row quarantines all of settings.ron.
fn parse_interval(hours: &str, minutes: &str) -> Result<u64, (&'static str, String)> {
    fn number(s: &str) -> Option<u64> {
        let s = s.trim();
        if s.is_empty() {
            Some(0)
        } else {
            s.parse().ok()
        }
    }
    let h = number(hours).ok_or_else(|| {
        (
            "interval_hours",
            format!("must be a whole number (got {hours:?})"),
        )
    })?;
    let m = number(minutes).filter(|m| *m < 60).ok_or_else(|| {
        (
            "interval_minutes",
            format!("must be 0–59 (got {minutes:?})"),
        )
    })?;
    let secs = h.saturating_mul(3600).saturating_add(m * 60);
    if secs > twitch_1337_core::schedule::MAX_INTERVAL.as_secs() {
        return Err((
            "interval",
            format!("must be at most 24 h (got {h} h {m} min)"),
        ));
    }
    Ok(secs)
}

fn parse_date(s: &str) -> Option<Date> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Date::strptime("%Y-%m-%d", s).ok()
}

fn parse_weekdays(values: &[String]) -> WeekdaySet {
    values.iter().filter_map(|v| parse_weekday(v)).collect()
}

#[derive(Debug, Deserialize)]
struct DeleteForm {
    _csrf: String,
}

async fn list(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Query(q): Query<ListQuery>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let rows = state.settings.load().schedules.clone();
    let now_utc = Timestamp::now();
    let runtimes = state.schedule_runtime.snapshot(now_utc).await;
    let now_berlin = to_berlin(now_utc);
    let stats = StatsView::new(&rows, &runtimes, &now_berlin);
    let edit_form = q.edit.as_ref().and_then(|name| {
        rows.iter()
            .find(|r| r.name == *name)
            .map(form_state_from_schedule)
    });
    let new_form = if q.new.unwrap_or(false) {
        // A new schedule fires unless the user opts out; an unchecked default
        // created rows that silently never ran.
        Some(FormState {
            enabled: true,
            ..FormState::default()
        })
    } else {
        None
    };
    let cards: Vec<CardView> = rows
        .iter()
        .map(|s| card_view(s, runtimes.get(&s.name), now_utc, &now_berlin))
        .collect();
    render(&ListTpl {
        rows: cards,
        edit_form,
        new_form,
        row_errors: Vec::new(),
        global_errors: Vec::new(),
        flash: flash::take(&cookies),
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::SCHEDULES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
        stats,
    })
}

fn card_view(
    s: &Schedule,
    runtime: Option<&twitch_1337_core::schedule::ScheduleRuntime>,
    now_utc: Timestamp,
    now_berlin: &ZonedDateTime,
) -> CardView {
    let (trigger_label, window_label) = trigger_human(&s.trigger);
    let last_fired_human = runtime.and_then(|r| r.last_fired_at).map(|t| {
        let delta = now_utc.duration_since(t);
        humanize_delta(delta)
    });
    let next_fire_human = if s.enabled {
        s.next_active_fire(now_berlin.clone())
            .map(|t| t.strftime("%a %H:%M").to_string())
    } else {
        None
    };
    CardView {
        name: s.name.clone(),
        message: s.message.clone(),
        ping: s.ping.clone(),
        enabled: s.enabled,
        trigger_label,
        schedule_window_label: window_label,
        last_fired_human,
        fires_today: runtime.map(|r| r.fires_today).unwrap_or(0),
        next_fire_human,
    }
}

fn trigger_human(t: &Trigger) -> (String, Option<String>) {
    match t {
        Trigger::Interval {
            every,
            days,
            active_from,
            active_to,
        } => {
            let secs = every.as_secs();
            let mut main = if secs % 3600 == 0 {
                format!("every {} h", secs / 3600)
            } else if secs >= 3600 {
                format!("every {}h{}m", secs / 3600, (secs / 60) % 60)
            } else {
                format!("every {} min", secs / 60)
            };
            if !days.is_empty() {
                main.push_str(&format!(" · {}", weekdays_short(days)));
            }
            let window = match (active_from, active_to) {
                (Some(f), Some(t2)) => {
                    Some(format!("{}–{}", f.strftime("%H:%M"), t2.strftime("%H:%M")))
                }
                _ => None,
            };
            (main, window)
        }
        Trigger::Calendar { days, at } => {
            let main = if days.is_empty() {
                format!("daily at {}", at.strftime("%H:%M"))
            } else {
                format!("{} at {}", weekdays_short(days), at.strftime("%H:%M"))
            };
            (main, None)
        }
    }
}

fn weekdays_short(days: &WeekdaySet) -> String {
    days.iter().map(weekday_abbr).collect::<Vec<_>>().join("·")
}

fn form_state_from_schedule(s: &Schedule) -> FormState {
    let mut fs = FormState {
        name: s.name.clone(),
        message: s.message.clone(),
        ping: s.ping.clone().unwrap_or_default(),
        enabled: s.enabled,
        start_date: s
            .start_date
            .map(|d| d.strftime("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        end_date: s
            .end_date
            .map(|d| d.strftime("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        ..FormState::default()
    };
    match &s.trigger {
        Trigger::Interval {
            every,
            days,
            active_from,
            active_to,
        } => {
            fs.kind = "interval".into();
            let total = every.as_secs();
            fs.interval_hours = (total / 3600).to_string();
            fs.interval_minutes = ((total / 60) % 60).to_string();
            fs.interval_active_from = active_from
                .map(|t| t.strftime("%H:%M").to_string())
                .unwrap_or_default();
            fs.interval_active_to = active_to
                .map(|t| t.strftime("%H:%M").to_string())
                .unwrap_or_default();
            fs.interval_days = days.iter().map(|d| weekday_abbr(d).to_owned()).collect();
        }
        Trigger::Calendar { days, at } => {
            fs.kind = "calendar".into();
            fs.calendar_at = at.strftime("%H:%M").to_string();
            fs.calendar_days = days.iter().map(|d| weekday_abbr(d).to_owned()).collect();
        }
    }
    fs
}

fn form_state_from_form(form: &ScheduleForm) -> FormState {
    FormState {
        name: form.name.trim().to_owned(),
        message: form.message.trim().to_owned(),
        ping: form.ping.trim().to_owned(),
        kind: form.kind.clone(),
        interval_hours: form.interval_hours.clone(),
        interval_minutes: form.interval_minutes.clone(),
        interval_active_from: form.interval_active_from.clone(),
        interval_active_to: form.interval_active_to.clone(),
        interval_days: form.interval_days.iter().cloned().collect(),
        calendar_at: form.calendar_at.clone(),
        calendar_days: form.calendar_days.iter().cloned().collect(),
        start_date: form.start_date.clone(),
        end_date: form.end_date.clone(),
        enabled: form.enabled.is_some(),
    }
}

async fn create(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum_extra::extract::Form(form): axum_extra::extract::Form<ScheduleForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let submitted = form_state_from_form(&form);
    let new_row = match form.try_into_schedule() {
        Ok(r) => r,
        Err(errs) => {
            return render_validation(
                &session,
                state.settings.load().schedules.clone(),
                errs,
                None,
                Some(submitted),
            );
        }
    };
    let new_row_for_mutate = new_row.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            let mut next = o.schedules.clone().unwrap_or_default();
            next.push(new_row_for_mutate);
            o.schedules = Some(next);
        }),
        None,
        Some(submitted),
    )
    .await
}

async fn update(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    cookies: Cookies,
    axum_extra::extract::Form(form): axum_extra::extract::Form<ScheduleForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let submitted = form_state_from_form(&form);
    let new_row = match form.try_into_schedule() {
        Ok(r) => r,
        Err(errs) => {
            return render_validation(
                &session,
                state.settings.load().schedules.clone(),
                errs,
                Some(name),
                Some(submitted),
            );
        }
    };
    let new_name = new_row.name.clone();
    let name_for_mutate = name.clone();
    let new_row_for_mutate = new_row.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            let mut next = o.schedules.clone().unwrap_or_default();
            if let Some(idx) = next.iter().position(|s| s.name == name_for_mutate) {
                next[idx] = new_row_for_mutate;
            }
            o.schedules = Some(next);
        }),
        Some(new_name),
        Some(submitted),
    )
    .await
}

async fn delete(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    cookies: Cookies,
    headers: HeaderMap,
    axum::Form(form): axum::Form<DeleteForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let mutate: Mutator = Box::new(move |o| {
        let next: Vec<Schedule> = o
            .schedules
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.name != name)
            .collect();
        o.schedules = Some(next);
    });
    if !is_htmx(&headers) {
        return apply_or_rerender(&state, &session, cookies, mutate, None, None).await;
    }
    match state
        .settings_store
        .apply_with(mutate, actor_of(&session))
        .await
    {
        Ok(_) => render(&swap_tpl(&state, &session, None).await),
        // No form to re-open from a card button; the toast carries the reason.
        Err(SettingsError::Validation(errs)) => Err(WebError::Validation {
            field: "schedules".into(),
            msg: errs
                .iter()
                .map(|e| format!("{}: {}", e.field, e.message))
                .collect::<Vec<_>>()
                .join("; "),
        }),
        Err(e) => Err(WebError::Internal(eyre::eyre!("settings apply: {e}"))),
    }
}

fn actor_of(session: &Session) -> Actor {
    Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    }
}

#[derive(Debug, Deserialize)]
struct ToggleForm {
    _csrf: String,
}

async fn toggle(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    cookies: Cookies,
    headers: HeaderMap,
    axum::Form(form): axum::Form<ToggleForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let name_for = name.clone();
    state
        .settings_store
        .apply_with(
            Box::new(
                move |o: &mut twitch_1337_core::settings::SettingsOverrides| {
                    let mut next = o.schedules.clone().unwrap_or_default();
                    if let Some(s) = next.iter_mut().find(|s| s.name == name_for) {
                        s.enabled.toggle();
                    }
                    o.schedules = Some(next);
                },
            ),
            actor_of(&session),
        )
        .await
        .map_err(|e| WebError::Internal(eyre::eyre!("settings apply: {e}")))?;
    if !is_htmx(&headers) {
        flash::set(&cookies, &format!("Toggled {name}."));
        return Ok(Redirect::to("/schedules").into_response());
    }
    // A schedule deleted in the meantime renders no card, so the swap drops it.
    render(&swap_tpl(&state, &session, Some(&name)).await)
}

type Mutator = Box<dyn FnOnce(&mut twitch_1337_core::settings::SettingsOverrides) + Send>;

async fn apply_or_rerender(
    state: &WebState,
    session: &Session,
    cookies: Cookies,
    mutate: Mutator,
    edit_on_error: Option<String>,
    submitted: Option<FormState>,
) -> Result<Response, WebError> {
    let actor = actor_of(session);
    match state.settings_store.apply_with(mutate, actor).await {
        Ok(_) => {
            flash::set(&cookies, "Schedules saved.");
            Ok(Redirect::to("/schedules").into_response())
        }
        Err(SettingsError::Validation(errs)) => {
            // Render the *persisted* rows as cards — never the rejected
            // attempt. A card for a row that was never saved vanishes the
            // moment the user clicks edit/toggle/delete on it (no such
            // schedule exists), trapping them. The attempt is instead
            // restored into the editable form via `submitted` so they can
            // fix and resubmit.
            render_validation(
                session,
                state.settings.load().schedules.clone(),
                errs,
                edit_on_error,
                submitted,
            )
        }
        Err(e) => Err(WebError::Internal(eyre::eyre!("settings apply: {e}"))),
    }
}

fn render_validation(
    session: &Session,
    rows: Vec<Schedule>,
    errs: Vec<twitch_1337_core::settings::FieldError>,
    edit_on_error: Option<String>,
    submitted: Option<FormState>,
) -> Result<Response, WebError> {
    let mut row_errors: Vec<RowError> = Vec::new();
    let mut global_errors: Vec<(String, String)> = Vec::new();
    for e in errs {
        if let Some(rest) = e.field.strip_prefix("schedules[")
            && let Some(end) = rest.find(']')
        {
            let idx_str = &rest[..end];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let field_name = rest.get(end + 2..).unwrap_or("").to_owned();
                if let Some(row) = rows.get(idx) {
                    row_errors.push(RowError {
                        row_index: idx,
                        row_name: row.name.clone(),
                        field: field_name,
                        message: e.message,
                    });
                    continue;
                }
                // Index past the persisted rows = the just-submitted row that
                // failed store validation and was never saved (e.g. a
                // duplicate name on create). It has no card, so attribute it to
                // the form rather than a phantom row that doesn't exist.
                global_errors.push((field_name, e.message));
                continue;
            }
        }
        global_errors.push((e.field, e.message));
    }
    // Rebuild card grid from the passed-in rows so the user can see existing
    // schedules alongside the error.
    let now_utc = Timestamp::now();
    let now_berlin = to_berlin(now_utc);
    // Schedule runtime state isn't loaded on this path, so "fired today" reads 0.
    let stats = StatsView::new(&rows, &HashMap::new(), &now_berlin);
    let cards: Vec<CardView> = rows
        .iter()
        .map(|s| card_view(s, None, now_utc, &now_berlin))
        .collect();

    // When re-rendering on validation failure, restore the user's submitted
    // form state (if available) for the edit or new form so they don't lose
    // their input.
    let (edit_form, new_form) = match (edit_on_error, submitted) {
        (Some(_), Some(fs)) => (Some(fs), None),
        (Some(name), None) => (
            Some(FormState {
                name,
                ..FormState::default()
            }),
            None,
        ),
        (None, Some(fs)) => (None, Some(fs)),
        (None, None) => (None, None),
    };

    let resp = ListTpl {
        rows: cards,
        edit_form,
        new_form,
        row_errors,
        global_errors,
        flash: None,
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::SCHEDULES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
        stats,
    };
    render(&resp)
}

#[cfg(test)]
mod try_into_schedule_tests {
    use super::{ScheduleForm, parse_hhmm, parse_interval, parse_weekdays};
    use twitch_1337_core::schedule::Trigger;

    fn base_interval_form() -> ScheduleForm {
        ScheduleForm {
            ping: String::new(),
            _csrf: "x".into(),
            name: "  noon  ".into(),
            message: "  hello world  ".into(),
            kind: "interval".into(),
            interval_hours: "1".into(),
            interval_minutes: "".into(),
            interval_active_from: "".into(),
            interval_active_to: "".into(),
            interval_days: vec![],
            calendar_at: "".into(),
            calendar_days: vec![],
            start_date: "".into(),
            end_date: "".into(),
            enabled: Some("true".into()),
        }
    }

    #[test]
    fn trims_text_fields() {
        let s = base_interval_form().try_into_schedule().unwrap();
        assert_eq!(s.name, "noon");
        assert_eq!(s.message, "hello world");
    }

    #[test]
    fn interval_over_a_day_rejected() {
        let mut form = base_interval_form();
        form.interval_hours = "24".into();
        form.interval_minutes = "1".into();
        let errs = form.try_into_schedule().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "form.interval"));
    }

    #[test]
    fn interval_of_exactly_a_day_accepted() {
        let mut form = base_interval_form();
        form.interval_hours = "24".into();
        assert!(form.try_into_schedule().is_ok());
    }

    #[test]
    fn interval_parsed_from_hours_and_minutes() {
        let mut form = base_interval_form();
        form.interval_minutes = "30".into();
        let s = form.try_into_schedule().unwrap();
        if let Trigger::Interval { every, .. } = s.trigger {
            assert_eq!(every.as_secs(), 5400);
        } else {
            panic!("expected Interval");
        }
    }

    #[test]
    fn interval_minutes_out_of_range_rejected() {
        let mut form = base_interval_form();
        form.interval_minutes = "60".into();
        let errs = form.try_into_schedule().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "form.interval_minutes"));
    }

    #[test]
    fn interval_hours_not_a_number_rejected() {
        let mut form = base_interval_form();
        form.interval_hours = "1:00".into();
        let errs = form.try_into_schedule().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "form.interval_hours"));
    }

    #[test]
    fn interval_zero_passes_through_for_downstream_validation() {
        // The form parser must NOT clamp; Schedule::validate is responsible
        // for rejecting intervals below 60s. Blank inputs read as 0 and must
        // produce a Duration of 0 without any silent correction.
        let mut form = base_interval_form();
        form.interval_hours = "".into();
        let s = form.try_into_schedule().unwrap();
        if let Trigger::Interval { every, .. } = s.trigger {
            assert_eq!(
                every.as_secs(),
                0,
                "form parser must not clamp; validate() handles this"
            );
        } else {
            panic!("expected Interval");
        }
    }

    #[test]
    fn calendar_at_required() {
        let form = ScheduleForm {
            ping: String::new(),
            _csrf: "x".into(),
            name: "test".into(),
            message: "hi".into(),
            kind: "calendar".into(),
            interval_hours: "".into(),
            interval_minutes: "".into(),
            interval_active_from: "".into(),
            interval_active_to: "".into(),
            interval_days: vec![],
            calendar_at: "".into(), // missing
            calendar_days: vec![],
            start_date: "".into(),
            end_date: "".into(),
            enabled: None,
        };
        let errs = form.try_into_schedule().unwrap_err();
        assert!(errs.iter().any(|e| e.field.contains("calendar_at")));
    }

    #[test]
    fn unknown_kind_returns_error() {
        let form = ScheduleForm {
            ping: String::new(),
            _csrf: "x".into(),
            name: "test".into(),
            message: "hi".into(),
            kind: "bogus".into(),
            interval_hours: "".into(),
            interval_minutes: "".into(),
            interval_active_from: "".into(),
            interval_active_to: "".into(),
            interval_days: vec![],
            calendar_at: "".into(),
            calendar_days: vec![],
            start_date: "".into(),
            end_date: "".into(),
            enabled: None,
        };
        let errs = form.try_into_schedule().unwrap_err();
        assert!(errs.iter().any(|e| e.field.contains("kind")));
    }

    #[test]
    fn parse_hhmm_helper() {
        assert_eq!(parse_hhmm("08:30"), Some(jiff::civil::time(8, 30, 0, 0)));
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(parse_hhmm("  "), None);
        assert_eq!(parse_hhmm("invalid"), None);
    }

    #[test]
    fn parse_interval_helper() {
        assert_eq!(parse_interval("1", "30"), Ok(5400));
        assert_eq!(parse_interval(" 2 ", ""), Ok(7200));
        assert_eq!(parse_interval("", ""), Ok(0));
        assert!(parse_interval("-1", "0").is_err());
    }

    #[test]
    fn parse_weekdays_helper() {
        let days = parse_weekdays(&["Mon".to_owned(), "Wed".to_owned()]);
        use jiff::civil::Weekday;
        assert!(days.contains(Weekday::Monday));
        assert!(days.contains(Weekday::Wednesday));
        assert!(!days.contains(Weekday::Friday));
    }
}

#[cfg(test)]
mod weekday_roundtrip_tests {
    use jiff::civil::Weekday;
    use twitch_1337_core::schedule::{parse_weekday, weekday_abbr};

    #[test]
    fn weekday_string_roundtrip_matches_template() {
        // `form_state_from_schedule` serialises weekdays via `weekday_abbr`.
        // The form parser deserialises them via `parse_weekday`.
        // Both ends must agree on the short-form strings the template renders.
        let cases = [
            (Weekday::Monday, "Mon"),
            (Weekday::Tuesday, "Tue"),
            (Weekday::Wednesday, "Wed"),
            (Weekday::Thursday, "Thu"),
            (Weekday::Friday, "Fri"),
            (Weekday::Saturday, "Sat"),
            (Weekday::Sunday, "Sun"),
        ];
        for (day, expected) in cases {
            let s = weekday_abbr(day);
            assert_eq!(s, expected, "abbr for {day:?} should be short form");
            let back = parse_weekday(s).expect("must round-trip");
            assert_eq!(back, day, "parse should recover the same weekday");
        }
    }
}
