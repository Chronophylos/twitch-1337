//! `/ai-profiles` CRUD (owner only — a profile carries `base_url`/`key`, the
//! same credential surface `/settings` keeps owner-only; see issue I1).
//!
//! Same shape as `/schedules`: a card grid plus an add/edit form, with
//! validation errors re-rendered against the *persisted* rows and the
//! rejected attempt restored into the form. Provider-specific quirks
//! (which `reasoning_effort` values exist, whether `service_tier` applies)
//! are never re-implemented here — they come from `AiProvider::caps()` and
//! the caps check that already runs inside `SettingsStore::apply_with` via
//! `settings::validation`.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::settings::{
    Actor, AiProfile, AiProvider, Efforts, FieldError, SettingsError,
};

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::routes::render;
use crate::state::WebState;

#[derive(Debug, Clone)]
struct RowError {
    row_index: usize,
    row_name: String,
    field: String,
    message: String,
}

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/ai-profiles", get(list))
        .route("/ai-profiles/add", post(create))
        .route("/ai-profiles/{name}/edit", post(update))
        .route("/ai-profiles/{name}/delete", post(delete))
        .route("/ai-profiles/{name}/activate", post(activate))
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    /// When `Some(name)`, the matching row renders in edit mode.
    edit: Option<String>,
    /// When `Some(true)`, the "new profile" form is shown.
    new: Option<bool>,
}

struct CardView {
    name: String,
    provider_label: &'static str,
    model: String,
    base_url: Option<String>,
    key: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    timeout: u64,
    is_active: bool,
}

/// One `<option>` for the provider `<select>`, plus enough of its
/// `ProviderCaps` for the form to render every provider's controls (all of
/// them, not just the currently-selected one — askama can't re-render on a
/// `<select>` change, so the template ships every variant's markup and a
/// small inline script toggles which one is visible/enabled).
struct ProviderOption {
    value: &'static str,
    label: &'static str,
    /// "fixed" | "free" | "none" — mirrors `Efforts` for template matching.
    effort_kind: &'static str,
    effort_values: Vec<&'static str>,
    service_tier: bool,
}

fn provider_options() -> Vec<ProviderOption> {
    AiProvider::ALL
        .into_iter()
        .map(|p| {
            let caps = p.caps();
            let (effort_kind, effort_values) = match caps.efforts {
                Efforts::Fixed(list) => ("fixed", list.to_vec()),
                Efforts::FreeText => ("free", Vec::new()),
                Efforts::None => ("none", Vec::new()),
            };
            ProviderOption {
                value: p.as_str(),
                label: p.label(),
                effort_kind,
                effort_values,
                service_tier: caps.service_tier,
            }
        })
        .collect()
}

#[derive(Default, Debug, Clone)]
struct FormState {
    name: String,
    provider: String,
    key: String,
    base_url: String,
    model: String,
    timeout: String,
    reasoning_effort: String,
    service_tier: String,
}

impl FormState {
    /// Defaults for a brand-new profile form: OpenRouter at the usual
    /// 30s timeout, everything else left for the user to fill in.
    fn new_default() -> Self {
        Self {
            provider: AiProvider::OpenRouter.as_str().to_owned(),
            timeout: "30".to_owned(),
            ..Self::default()
        }
    }
}

#[derive(Template)]
#[template(path = "ai_profiles/index.html")]
struct ListTpl {
    rows: Vec<CardView>,
    edit_form: Option<FormState>,
    /// The persisted name the edit form's POST must target — always the
    /// name the row was opened under, never whatever the user has typed
    /// into the rename field. `edit_form.name` is restored from the
    /// user's submission (so a rejected rename isn't lost from the input),
    /// but the form `action` must keep pointing at the row that actually
    /// exists; deriving it from the submitted name instead sends the fix-up
    /// POST to a profile that was never created, and the mutate closure
    /// silently no-ops (see `update`'s regression test).
    edit_target: Option<String>,
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
    total: usize,
    active_profile: String,
    providers: Vec<ProviderOption>,
}

#[derive(Debug, Deserialize)]
struct ProfileForm {
    _csrf: String,
    name: String,
    provider: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    base_url: String,
    model: String,
    timeout: String,
    #[serde(default)]
    reasoning_effort: String,
    #[serde(default)]
    service_tier: String,
}

impl ProfileForm {
    /// Parses the raw form fields into an `AiProfile`. Deliberately does
    /// NOT re-check provider caps (e.g. whether `reasoning_effort` is
    /// allowed) — that check already lives in `settings::validation` and
    /// comes back through `SettingsError::Validation` from the store.
    fn to_profile(&self) -> Result<AiProfile, FieldError> {
        let provider = AiProvider::ALL
            .into_iter()
            .find(|p| p.as_str() == self.provider)
            .ok_or_else(|| FieldError {
                field: "form.provider".into(),
                message: format!("unknown provider {:?}", self.provider),
            })?;
        let timeout: u64 = self.timeout.trim().parse().map_err(|_| FieldError {
            field: "form.timeout".into(),
            message: format!("must be a whole number of seconds (got {:?})", self.timeout),
        })?;
        Ok(AiProfile {
            name: self.name.trim().to_owned(),
            key: non_empty(&self.key),
            provider,
            base_url: non_empty(&self.base_url),
            model: self.model.trim().to_owned(),
            timeout,
            reasoning_effort: non_empty(&self.reasoning_effort),
            service_tier: non_empty(&self.service_tier),
        })
    }
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

#[derive(Debug, Deserialize)]
struct DeleteForm {
    _csrf: String,
}

#[derive(Debug, Deserialize)]
struct ActivateForm {
    _csrf: String,
}

async fn list(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Query(q): Query<ListQuery>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let settings = state.settings.load();
    let rows = settings.ai.profiles.clone();
    let active = settings.ai.active_profile.clone();
    let edit_form = q.edit.as_ref().and_then(|name| {
        rows.iter()
            .find(|p| p.name == *name)
            .map(form_state_from_profile)
    });
    let edit_target = edit_form.is_some().then(|| q.edit.clone()).flatten();
    let new_form = if q.new.unwrap_or(false) {
        Some(FormState::new_default())
    } else {
        None
    };
    let total = rows.len();
    let cards: Vec<CardView> = rows.iter().map(|p| card_view(p, &active)).collect();
    render(&ListTpl {
        rows: cards,
        edit_form,
        edit_target,
        new_form,
        row_errors: Vec::new(),
        global_errors: Vec::new(),
        flash: flash::take(&cookies),
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::AI_PROFILES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
        total,
        active_profile: active,
        providers: provider_options(),
    })
}

fn card_view(p: &AiProfile, active: &str) -> CardView {
    CardView {
        name: p.name.clone(),
        provider_label: p.provider.label(),
        model: p.model.clone(),
        base_url: p.base_url.clone(),
        key: p.key.clone(),
        reasoning_effort: p.reasoning_effort.clone(),
        service_tier: p.service_tier.clone(),
        timeout: p.timeout,
        is_active: p.name == active,
    }
}

fn form_state_from_profile(p: &AiProfile) -> FormState {
    FormState {
        name: p.name.clone(),
        provider: p.provider.as_str().to_owned(),
        key: p.key.clone().unwrap_or_default(),
        base_url: p.base_url.clone().unwrap_or_default(),
        model: p.model.clone(),
        timeout: p.timeout.to_string(),
        reasoning_effort: p.reasoning_effort.clone().unwrap_or_default(),
        service_tier: p.service_tier.clone().unwrap_or_default(),
    }
}

fn form_state_from_form(form: &ProfileForm) -> FormState {
    FormState {
        name: form.name.trim().to_owned(),
        provider: form.provider.clone(),
        key: form.key.clone(),
        base_url: form.base_url.clone(),
        model: form.model.trim().to_owned(),
        timeout: form.timeout.clone(),
        reasoning_effort: form.reasoning_effort.clone(),
        service_tier: form.service_tier.clone(),
    }
}

async fn create(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum_extra::extract::Form(form): axum_extra::extract::Form<ProfileForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let submitted = form_state_from_form(&form);
    let new_profile = match form.to_profile() {
        Ok(p) => p,
        Err(e) => {
            let settings = state.settings.load();
            return render_validation(
                &session,
                settings.ai.profiles.clone(),
                settings.ai.active_profile.clone(),
                vec![e],
                None,
                Some(submitted),
            );
        }
    };
    let current_profiles = state.settings.load().ai.profiles.clone();
    let new_row = new_profile.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            let mut next =
                o.ai.profiles
                    .clone()
                    .unwrap_or_else(|| current_profiles.clone());
            next.push(new_row.clone());
            o.ai.profiles = Some(next);
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
    axum_extra::extract::Form(form): axum_extra::extract::Form<ProfileForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let submitted = form_state_from_form(&form);
    let edited = match form.to_profile() {
        Ok(p) => p,
        Err(e) => {
            let settings = state.settings.load();
            return render_validation(
                &session,
                settings.ai.profiles.clone(),
                settings.ai.active_profile.clone(),
                vec![e],
                Some(name),
                Some(submitted),
            );
        }
    };
    let target = name.clone();
    let edit_on_error = name.clone();
    let settings = state.settings.load();
    let current_profiles = settings.ai.profiles.clone();
    let active_now = settings.ai.active_profile.clone();
    let edited_for_mutate = edited.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            let mut next =
                o.ai.profiles
                    .clone()
                    .unwrap_or_else(|| current_profiles.clone());
            if let Some(slot) = next.iter_mut().find(|p| p.name == target) {
                *slot = edited_for_mutate.clone();
            }
            // Renaming the active profile must carry the pointer with it,
            // otherwise the save fails validation on a dangling
            // `active_profile`.
            let active_ptr =
                o.ai.active_profile
                    .clone()
                    .unwrap_or_else(|| active_now.clone());
            if active_ptr == target {
                o.ai.active_profile = Some(edited_for_mutate.name.clone());
            }
            o.ai.profiles = Some(next);
        }),
        Some(edit_on_error),
        Some(submitted),
    )
    .await
}

async fn delete(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    cookies: Cookies,
    axum::Form(form): axum::Form<DeleteForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let settings = state.settings.load();
    let profiles = settings.ai.profiles.clone();
    let active = settings.ai.active_profile.clone();
    if profiles.len() == 1 {
        return render_validation(
            &session,
            profiles,
            active,
            vec![FieldError {
                field: "ai.profiles".into(),
                message: "at least one profile is required".into(),
            }],
            None,
            None,
        );
    }
    if name == active {
        return render_validation(
            &session,
            profiles,
            active,
            vec![FieldError {
                field: "ai.active_profile".into(),
                message: "switch to another profile before deleting this one".into(),
            }],
            None,
            None,
        );
    }
    let target = name.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            let mut next = o.ai.profiles.clone().unwrap_or_else(|| profiles.clone());
            next.retain(|p| p.name != target);
            o.ai.profiles = Some(next);
            // A deleted profile can't stay pinned by a sub-agent — the save
            // would fail validation on a dangling `*.profile` pointer.
            for slot in [
                &mut o.ai.dreamer.profile,
                &mut o.ai.grok.profile,
                &mut o.ai.media.profile,
            ] {
                if slot.as_ref().and_then(|v| v.as_deref()) == Some(target.as_str()) {
                    *slot = Some(None);
                }
            }
        }),
        None,
        None,
    )
    .await
}

async fn activate(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    cookies: Cookies,
    axum::Form(form): axum::Form<ActivateForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            o.ai.active_profile = Some(name.clone());
        }),
        None,
        None,
    )
    .await
}

type Mutator =
    Box<dyn FnOnce(&mut twitch_1337_core::settings::overrides::SettingsOverrides) + Send>;

async fn apply_or_rerender(
    state: &WebState,
    session: &Session,
    cookies: Cookies,
    mutate: Mutator,
    edit_on_error: Option<String>,
    submitted: Option<FormState>,
) -> Result<Response, WebError> {
    let actor = Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    };
    match state.settings_store.apply_with(mutate, actor).await {
        Ok(_) => {
            flash::set(&cookies, "AI profiles saved.");
            Ok(Redirect::to("/ai-profiles").into_response())
        }
        Err(SettingsError::Validation(errs)) => {
            // Render the *persisted* rows as cards — never the rejected
            // attempt (see the identical note in `routes/schedules.rs`).
            let settings = state.settings.load();
            render_validation(
                session,
                settings.ai.profiles.clone(),
                settings.ai.active_profile.clone(),
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
    rows: Vec<AiProfile>,
    active: String,
    errs: Vec<FieldError>,
    edit_on_error: Option<String>,
    submitted: Option<FormState>,
) -> Result<Response, WebError> {
    let mut row_errors: Vec<RowError> = Vec::new();
    let mut global_errors: Vec<(String, String)> = Vec::new();
    for e in errs {
        if let Some(rest) = e.field.strip_prefix("ai.profiles[")
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
                // Index past the persisted rows = the just-submitted row
                // that failed store validation and was never saved. It has
                // no card, so attribute it to the form rather than a
                // phantom row that doesn't exist.
                global_errors.push((field_name, e.message));
                continue;
            }
        }
        global_errors.push((e.field, e.message));
    }
    let total = rows.len();
    let cards: Vec<CardView> = rows.iter().map(|p| card_view(p, &active)).collect();

    // The edit form's POST target must stay pinned to the row this edit
    // actually opened against, regardless of what name the user typed into
    // the rename field — `edit_on_error` carries that original name.
    let edit_target = edit_on_error.clone();
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
        edit_target,
        new_form,
        row_errors,
        global_errors,
        flash: None,
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::AI_PROFILES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
        total,
        active_profile: active,
        providers: provider_options(),
    };
    render(&resp)
}
