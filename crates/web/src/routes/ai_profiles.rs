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

use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use llm::LlmError;
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::llm_factory::ProfileTestError;
use twitch_1337_core::settings::{
    Actor, AiProfile, AiProvider, Efforts, FieldError, ProfileKind, Settings, SettingsError,
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
        .route("/ai-profiles/test", post(test_connection))
        .route("/ai-profiles/{name}/edit", post(update))
        .route("/ai-profiles/{name}/delete", post(delete))
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    /// When `Some(name)`, the matching row renders in edit mode.
    edit: Option<String>,
    /// When `Some(true)`, the "new profile" form is shown.
    new: Option<bool>,
}

struct ConnectionTestView {
    success: bool,
    message: String,
}

struct CardView {
    name: String,
    kind: &'static str,
    provider_label: &'static str,
    model: String,
    base_url: Option<String>,
    key: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    timeout: u64,
}

/// One `<option>` for the provider `<select>`, plus enough of its
/// `ProviderCaps` for the form to render every provider's controls (all of
/// them, not just the currently-selected one — askama can't re-render on a
/// `<select>` change, so the template ships every variant's markup and
/// `app.js` toggles which one is visible/enabled).
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
    /// `ProfileKind::as_str()`; empty is treated as chat.
    kind: String,
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
            kind: ProfileKind::Chat.as_str().to_owned(),
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
    connection_test: Option<ConnectionTestView>,
    csrf: String,
    user_login: String,
    user_avatar_url: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
    total: usize,
    providers: Vec<ProviderOption>,
    kinds: [ProfileKind; 2],
}

#[derive(Debug, Deserialize)]
struct ProfileForm {
    _csrf: String,
    #[serde(default)]
    _edit_target: Option<String>,
    name: String,
    #[serde(default)]
    kind: String,
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
    /// Parses the raw form fields into an `AiProfile`. Provider caps
    /// (effort/tier) are left to `settings::validation` via the store;
    /// the key-name requirement lives here so a missing name cannot
    /// quarantine `settings.ron` on bot start.
    fn to_profile(&self) -> Result<AiProfile, FieldError> {
        let provider = AiProvider::ALL
            .into_iter()
            .find(|p| p.as_str() == self.provider)
            .ok_or_else(|| FieldError {
                field: "form.provider".into(),
                message: format!("unknown provider {:?}", self.provider),
            })?;
        let kind = if self.kind.trim().is_empty() {
            ProfileKind::Chat
        } else {
            ProfileKind::ALL
                .into_iter()
                .find(|k| k.as_str() == self.kind.trim())
                .ok_or_else(|| FieldError {
                    field: "form.kind".into(),
                    message: format!("unknown profile kind {:?}", self.kind),
                })?
        };
        #[expect(
            clippy::map_err_ignore,
            reason = "integer parse failure is fully described by the echoed input"
        )]
        let timeout: u64 = self.timeout.trim().parse().map_err(|_| FieldError {
            field: "form.timeout".into(),
            message: format!("must be a whole number of seconds (got {:?})", self.timeout),
        })?;
        let key = non_empty(&self.key);
        if provider.uses_openai_client() && !self.model.trim().is_empty() && key.is_none() {
            return Err(FieldError {
                field: "form.key".into(),
                message: "required (must name an [ai.keys] entry)".into(),
            });
        }
        Ok(AiProfile {
            name: self.name.trim().to_owned(),
            key,
            provider,
            base_url: non_empty(&self.base_url),
            model: self.model.trim().to_owned(),
            timeout,
            reasoning_effort: non_empty(&self.reasoning_effort),
            service_tier: non_empty(&self.service_tier),
            kind,
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

async fn list(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Query(q): Query<ListQuery>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let settings = state.settings.load();
    let rows = settings.ai.profiles.clone();
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
    let cards: Vec<CardView> = rows.iter().map(card_view).collect();
    render(&ListTpl {
        rows: cards,
        edit_form,
        edit_target,
        new_form,
        row_errors: Vec::new(),
        global_errors: Vec::new(),
        flash: flash::take(&cookies),
        connection_test: None,
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::AI_PROFILES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
        total,
        providers: provider_options(),
        kinds: ProfileKind::ALL,
    })
}

fn card_view(p: &AiProfile) -> CardView {
    CardView {
        name: p.name.clone(),
        kind: p.kind.as_str(),
        provider_label: p.provider.label(),
        model: p.model.clone(),
        base_url: p.base_url.clone(),
        key: p.key.clone(),
        reasoning_effort: p.reasoning_effort.clone(),
        service_tier: p.service_tier.clone(),
        timeout: p.timeout,
    }
}

fn form_state_from_profile(p: &AiProfile) -> FormState {
    FormState {
        name: p.name.clone(),
        kind: p.kind.as_str().to_owned(),
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
        kind: form.kind.clone(),
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

async fn test_connection(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    axum_extra::extract::Form(form): axum_extra::extract::Form<ProfileForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let edit_target = form._edit_target.clone();
    let submitted = form_state_from_form(&form);
    let profile = match form.to_profile() {
        Ok(profile) => profile,
        Err(error) => {
            let settings = state.settings.load();
            return render_validation(
                &session,
                settings.ai.profiles.clone(),
                vec![error],
                edit_target,
                Some(submitted),
            );
        }
    };

    let connection_test = match state.llm.test_profile(&profile).await {
        Ok(latency) => ConnectionTestView {
            success: true,
            message: format!(
                "Connection successful in {} ms. The model returned a response.",
                latency.as_millis()
            ),
        },
        Err(error) => ConnectionTestView {
            success: false,
            message: connection_failure(error),
        },
    };
    render_page(
        &session,
        state.settings.load().ai.profiles.clone(),
        Vec::new(),
        edit_target,
        Some(submitted),
        Some(connection_test),
    )
}

fn connection_failure(error: ProfileTestError) -> String {
    match error {
        ProfileTestError::UnknownKey(name) => format!(
            "Key name {name:?} has no matching [ai.keys] entry in config.toml. Add the entry or choose another key name."
        ),
        ProfileTestError::MissingKey => {
            "This provider requires a key name from [ai.keys].".to_owned()
        }
        ProfileTestError::Llm(LlmError::Provider { status, body }) => {
            let summary = match status {
                401 | 403 => format!(
                    "Authentication failed (HTTP {status}). Check the selected [ai.keys] entry and its provider permissions."
                ),
                404 => format!(
                    "Model or endpoint not found (HTTP {status}). Check Model and Base URL."
                ),
                400 | 422 => format!(
                    "Provider rejected the request (HTTP {status}). Check Model, Reasoning effort, and Service tier."
                ),
                429 => "Provider rate limited the test (HTTP 429). Try again later.".to_owned(),
                500..=599 => format!(
                    "Provider failed the request (HTTP {status}). Check the provider's status and try again."
                ),
                _ => format!("Provider rejected the request (HTTP {status})."),
            };
            format!("{summary}{}", provider_excerpt(&body))
        }
        ProfileTestError::Llm(LlmError::Http(error)) if error.is_timeout() => {
            "Request timed out. Increase Timeout or check provider load.".to_owned()
        }
        ProfileTestError::Llm(LlmError::Http(error)) if error.is_connect() => {
            "Could not connect to the provider. Check Base URL, DNS, and TLS.".to_owned()
        }
        ProfileTestError::Llm(LlmError::Http(error)) if error.is_builder() => {
            "Could not build the provider request. Check Base URL.".to_owned()
        }
        ProfileTestError::Llm(LlmError::Http(_)) => {
            "Could not send the request to the provider. Check Base URL and network access."
                .to_owned()
        }
        ProfileTestError::Llm(LlmError::Header(_)) => {
            "The selected API key cannot be used in an HTTP Authorization header. Check [ai.keys]."
                .to_owned()
        }
        ProfileTestError::Llm(LlmError::Decode { stage, .. }) => {
            format!("Provider returned an unreadable response during {stage}.")
        }
        ProfileTestError::Llm(LlmError::EmptyResponse) => {
            "Provider returned an empty response. Check that the model supports chat completions."
                .to_owned()
        }
        ProfileTestError::Llm(LlmError::Timeout) => {
            "Request timed out. Increase Timeout or check provider load.".to_owned()
        }
    }
}

fn provider_excerpt(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return String::new();
    }
    let mut chars = body.chars();
    let excerpt: String = chars.by_ref().take(600).collect();
    let truncated = if chars.next().is_some() { "…" } else { "" };
    format!(" Provider response: {excerpt}{truncated}")
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
                vec![e],
                Some(name),
                Some(submitted),
            );
        }
    };
    let target = name.clone();
    let edit_on_error = name.clone();
    let defaults = state.settings_store.defaults().clone();
    let edited_for_mutate = edited.clone();
    apply_or_rerender(
        &state,
        &session,
        cookies,
        Box::new(move |o| {
            // Resolve from the overrides loaded under SettingsStore's lock.
            // A concurrent settings save may have repointed an agent since
            // this HTTP request began; rename only pointers that are current.
            let current = Settings::resolve(&defaults, o);
            let rewrite_aurora = current.ai.aurora.profile == target;
            let rewrite_grok = current.ai.grok.profile == target;
            let rewrite_dreamer = current.ai.dreamer.profile == target;
            let rewrite_media = current.ai.media.profile == target;
            let rewrite_recap = current.ai.recap.profile.as_deref() == Some(target.as_str());
            let rewrite_spoiler = current.spoiler.profile == target;
            let mut next = current.ai.profiles;
            if let Some(slot) = next.iter_mut().find(|p| p.name == target) {
                *slot = edited_for_mutate.clone();
            }
            let renamed = edited_for_mutate.name.clone();
            if rewrite_aurora {
                o.ai.aurora.profile = Some(Some(renamed.clone()));
            }
            if rewrite_grok {
                o.ai.grok.profile = Some(Some(renamed.clone()));
            }
            if rewrite_dreamer {
                o.ai.dreamer.profile = Some(Some(renamed.clone()));
            }
            if rewrite_media {
                o.ai.media.profile = Some(Some(renamed.clone()));
            }
            if rewrite_recap {
                o.ai.recap.profile = Some(Some(renamed.clone()));
            }
            if rewrite_spoiler {
                o.spoiler.profile = Some(renamed);
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
    if profiles.len() == 1 {
        return render_validation(
            &session,
            profiles,
            vec![FieldError {
                field: "ai.profiles".into(),
                message: "at least one profile is required".into(),
            }],
            None,
            None,
        );
    }
    let in_use = [
        settings.ai.aurora.profile.as_str(),
        settings.ai.grok.profile.as_str(),
        settings.ai.dreamer.profile.as_str(),
        settings.ai.media.profile.as_str(),
        settings.spoiler.profile.as_str(),
    ]
    .into_iter()
    .any(|profile| profile == name)
        || settings.ai.recap.profile.as_deref() == Some(name.as_str());
    if in_use {
        return render_validation(
            &session,
            profiles,
            vec![FieldError {
                field: "ai.profiles".into(),
                message: "assign every agent and the spoiler check to another profile before deleting this one".into(),
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
        }),
        None,
        None,
    )
    .await
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
    errs: Vec<FieldError>,
    edit_on_error: Option<String>,
    submitted: Option<FormState>,
) -> Result<Response, WebError> {
    render_page(session, rows, errs, edit_on_error, submitted, None)
}

fn render_page(
    session: &Session,
    rows: Vec<AiProfile>,
    errs: Vec<FieldError>,
    edit_on_error: Option<String>,
    submitted: Option<FormState>,
    connection_test: Option<ConnectionTestView>,
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
    let cards: Vec<CardView> = rows.iter().map(card_view).collect();

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
        connection_test,
        csrf: csrf::encode(&session.csrf_value),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::AI_PROFILES,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, crate::auth::Role::Owner),
        total,
        providers: provider_options(),
        kinds: ProfileKind::ALL,
    };
    render(&resp)
}
