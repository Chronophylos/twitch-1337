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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use llm::LlmError;
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::llm_factory::{ListedModel, ModelListingError, ProfileTestError};
use twitch_1337_core::settings::{
    Actor, AiProfile, AiProvider, Efforts, FieldError, ProfileKind, ProviderCaps, Settings,
    SettingsError,
};

use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::routes::render;
use crate::state::WebState;

/// Inline errors for the profile form, one slot per control. A slot is
/// `Some` only when the control is rendered visibly on the re-rendered
/// page; errors for hidden controls go to the page summary instead.
#[derive(Debug, Default)]
struct FormErrors {
    name: Option<String>,
    kind: Option<String>,
    provider: Option<String>,
    key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    timeout: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

impl FormErrors {
    fn slot(&mut self, field: &str) -> Option<&mut Option<String>> {
        Some(match field {
            "name" => &mut self.name,
            "kind" => &mut self.kind,
            "provider" => &mut self.provider,
            "key" => &mut self.key,
            "base_url" => &mut self.base_url,
            "model" => &mut self.model,
            "timeout" => &mut self.timeout,
            "reasoning_effort" => &mut self.reasoning_effort,
            "service_tier" => &mut self.service_tier,
            _ => return None,
        })
    }

    fn any(&self) -> bool {
        self.name.is_some()
            || self.kind.is_some()
            || self.provider.is_some()
            || self.key.is_some()
            || self.base_url.is_some()
            || self.model.is_some()
            || self.advanced()
    }

    /// Whether an error sits inside the collapsible **Advanced** section,
    /// which must then render open.
    fn advanced(&self) -> bool {
        self.timeout.is_some() || self.reasoning_effort.is_some() || self.service_tier.is_some()
    }
}

/// Human label for a profile field, used when its error has no inline
/// control and lands in the page summary.
fn field_label(field: &str) -> &str {
    match field {
        "name" => "Name",
        "kind" => "Kind",
        "provider" => "Provider",
        "key" => "Key name",
        "base_url" => "Base URL",
        "model" => "Model",
        "timeout" => "Timeout",
        "reasoning_effort" => "Reasoning effort",
        "service_tier" => "Service tier",
        other => other,
    }
}

/// Whether the re-rendered form shows `field`'s control to the user. Mirrors
/// the template (key row hidden for providers without an OpenAI client,
/// effort/tier only in the selected provider's group) and `app.js` (decision
/// profiles hide every provider group).
fn control_visible(field: &str, form: &FormState) -> bool {
    let provider = AiProvider::ALL
        .into_iter()
        .find(|p| p.as_str() == form.provider);
    let groups_shown = form.kind.trim() != ProfileKind::Decision.as_str();
    match field {
        "name" | "kind" | "provider" | "base_url" | "model" | "timeout" => true,
        "key" => provider.is_none_or(AiProvider::uses_openai_client),
        "reasoning_effort" => {
            groups_shown && provider.is_some_and(|p| !matches!(p.caps().efforts, Efforts::None))
        }
        "service_tier" => groups_shown && provider.is_some_and(|p| p.caps().service_tier),
        _ => false,
    }
}

/// Agent pointers that reference profiles from other settings sections.
fn reference_label(field: &str) -> Option<&'static str> {
    Some(match field {
        "ai.aurora.profile" => "Aurora",
        "ai.grok.profile" => "Grok",
        "ai.dreamer.profile" => "Dreamer",
        "ai.emotes_agent.profile" => "Emotes agent",
        "ai.media.profile" => "Media",
        "ai.recap.profile" => "Recap",
        "spoiler.profile" => "Spoiler check",
        _ => return None,
    })
}

/// The submitted form a re-render reports errors against: its values and
/// the `ai.profiles` index it occupies in the list the store validated
/// (the edited row, or one past the persisted rows for a new profile).
struct SubmittedRow<'a> {
    form: &'a FormState,
    index: Option<usize>,
}

/// Errors split by where they render: inline next to a form control, or in
/// the page summary as a sentence naming the profile/agent involved.
#[derive(Debug, Default)]
struct PageErrors {
    form: FormErrors,
    global: Vec<String>,
}

impl PageErrors {
    fn push_form_field(&mut self, field: &str, message: String, form: &FormState) {
        if control_visible(field, form)
            && let Some(slot) = self.form.slot(field)
        {
            *slot = Some(match slot.take() {
                Some(previous) => format!("{previous}; {message}"),
                None => message,
            });
            return;
        }
        self.global
            .push(format!("{}: {message}", field_label(field)));
    }
}

/// Sorts `FieldError`s from `ProfileForm::to_profile` (`form.<field>`) and
/// `settings::validation` (`ai.profiles[<idx>].<field>`, `ai.profiles`,
/// agent pointers) into inline form errors and summary sentences. Summary
/// sentences never expose storage paths or row indices.
fn classify_errors(
    errs: Vec<FieldError>,
    rows: &[AiProfile],
    submitted: Option<&SubmittedRow<'_>>,
) -> PageErrors {
    let mut out = PageErrors::default();
    for FieldError { field, message } in errs {
        if let Some(f) = field.strip_prefix("form.") {
            match submitted {
                Some(s) => out.push_form_field(f, message, s.form),
                None => out.global.push(format!("{}: {message}", field_label(f))),
            }
            continue;
        }
        if let Some(rest) = field.strip_prefix("ai.profiles[")
            && let Some((idx, f)) = rest.split_once("].")
            && let Ok(idx) = idx.parse::<usize>()
        {
            if let Some(s) = submitted {
                // A rename onto an existing name is flagged on whichever of
                // the two rows comes later; when that is the other profile,
                // the fix still belongs in this form's Name input.
                let duplicate_of_submitted =
                    f == "name" && rows.get(idx).is_some_and(|p| p.name == s.form.name);
                if s.index == Some(idx) || duplicate_of_submitted {
                    out.push_form_field(f, message, s.form);
                    continue;
                }
            }
            out.global.push(match rows.get(idx) {
                Some(p) => format!("Profile {:?}, {}: {message}", p.name, field_label(f)),
                None => format!("{}: {message}", field_label(f)),
            });
            continue;
        }
        if field == "ai.profiles" {
            out.global.push(capitalize(&message));
            continue;
        }
        out.global.push(match reference_label(&field) {
            Some(label) => format!("{label}: {message}"),
            None => format!("Setting {field}: {message}"),
        });
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/ai-profiles", get(list))
        .route("/ai-profiles/add", post(create))
        .route("/ai-profiles/test", post(test_connection))
        .route("/ai-profiles/models", post(list_models))
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

struct AssignmentView {
    label: &'static str,
    settings_anchor: &'static str,
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
    assignments: Vec<AssignmentView>,
    assignment_names: String,
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
    model_listing: bool,
    requires_key: bool,
    base_url_required: bool,
    default_base_url: Option<&'static str>,
    model_placeholder: &'static str,
}

fn provider_options() -> Vec<ProviderOption> {
    AiProvider::ALL
        .into_iter()
        .map(|provider| provider_option_with_caps(provider, provider.caps()))
        .collect()
}

fn provider_option_with_caps(provider: AiProvider, caps: ProviderCaps) -> ProviderOption {
    let (effort_kind, effort_values) = match caps.efforts {
        Efforts::Fixed(list) => ("fixed", list.to_vec()),
        Efforts::FreeText => ("free", Vec::new()),
        Efforts::None => ("none", Vec::new()),
    };
    ProviderOption {
        value: provider.as_str(),
        label: provider.label(),
        effort_kind,
        effort_values,
        service_tier: caps.service_tier,
        model_listing: caps.supports_model_listing(),
        requires_key: provider.uses_openai_client(),
        base_url_required: caps.default_base_url.is_none(),
        default_base_url: caps.default_base_url,
        model_placeholder: match provider {
            AiProvider::OpenRouter => "provider/model-id",
            AiProvider::OpenAi => "gpt-5",
            AiProvider::OpenAiCompatible => "vendor/model-id",
            AiProvider::Ollama => "qwen3:8b",
        },
    }
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
#[template(path = "ai_profiles/_model_options.html")]
struct ModelOptionsTpl {
    id: &'static str,
    models: Vec<ListedModel>,
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
    /// Errors shown beside the open form's controls.
    form_errors: FormErrors,
    /// Errors with no control on the form, already worded for humans.
    global_errors: Vec<String>,
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
        self.to_profile_inner(true)
    }

    /// Listing deliberately lets the registry report a missing key, just like
    /// its other operational failures, rather than re-rendering the form.
    fn to_listing_profile(&self) -> Result<AiProfile, FieldError> {
        self.to_profile_inner(false)
    }

    fn to_profile_inner(&self, require_key: bool) -> Result<AiProfile, FieldError> {
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
        if require_key
            && provider.uses_openai_client()
            && !self.model.trim().is_empty()
            && key.is_none()
        {
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
    let cards: Vec<CardView> = rows.iter().map(|p| card_view(p, &settings)).collect();
    render(&ListTpl {
        rows: cards,
        edit_form,
        edit_target,
        new_form,
        form_errors: FormErrors::default(),
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

/// Every agent / the spoiler check whose profile pointer names `name`.
fn assignments(name: &str, settings: &Settings) -> Vec<AssignmentView> {
    let mut assignments = Vec::new();
    for (label, settings_anchor, selected) in [
        (
            "Aurora",
            "sec-ai-aurora",
            settings.ai.aurora.profile.as_str(),
        ),
        ("Grok", "sec-ai-grok", settings.ai.grok.profile.as_str()),
        (
            "Dreamer",
            "sec-ai-dreamer",
            settings.ai.dreamer.profile.as_str(),
        ),
        (
            "Emotes",
            "sec-ai-emotes-agent",
            settings.ai.emotes_agent.profile.as_str(),
        ),
        ("Media", "sec-ai-media", settings.ai.media.profile.as_str()),
        (
            "Recap",
            "sec-ai-recap",
            settings.ai.recap.profile.as_deref().unwrap_or(""),
        ),
        (
            "Spoiler check",
            "sec-spoiler",
            settings.spoiler.profile.as_str(),
        ),
    ] {
        if selected == name {
            assignments.push(AssignmentView {
                label,
                settings_anchor,
            });
        }
    }
    assignments
}

fn assignment_names(assignments: &[AssignmentView]) -> String {
    assignments
        .iter()
        .map(|assignment| assignment.label)
        .collect::<Vec<_>>()
        .join(", ")
}

fn card_view(p: &AiProfile, settings: &Settings) -> CardView {
    let assignments = assignments(&p.name, settings);
    let assignment_names = assignment_names(&assignments);

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
        assignments,
        assignment_names,
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
            return render_validation(&state, &session, vec![e], None, Some(submitted));
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
            return render_validation(&state, &session, vec![error], edit_target, Some(submitted));
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
        &state,
        &session,
        Vec::new(),
        edit_target,
        Some(submitted),
        Some(connection_test),
    )
}

async fn list_models(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    axum_extra::extract::Form(form): axum_extra::extract::Form<ProfileForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form._csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let profile = match form.to_listing_profile() {
        Ok(profile) => profile,
        Err(error) => {
            let message = match error.field.as_str() {
                "form.key" => "This provider requires a key name from [ai.keys].",
                "form.timeout" => "Timeout must be a whole number of seconds.",
                "form.provider" => "Unknown provider.",
                "form.kind" => "Unknown profile kind.",
                _ => "Invalid profile fields; check Provider, Key name, and Timeout.",
            };
            return Ok((StatusCode::BAD_REQUEST, message).into_response());
        }
    };
    match state.llm.list_models(&profile).await {
        Ok(models) => render(&ModelOptionsTpl {
            id: if form._edit_target.is_some() {
                "profile-edit-model-options"
            } else {
                "profile-new-model-options"
            },
            models,
        }),
        Err(error) => {
            let (status, message) = model_listing_failure(error);
            Ok((status, message).into_response())
        }
    }
}

fn model_listing_failure(error: ModelListingError) -> (StatusCode, String) {
    match error {
        ModelListingError::UnknownKey => (
            StatusCode::BAD_REQUEST,
            "Unknown key name. Choose an existing [ai.keys] entry.".to_owned(),
        ),
        ModelListingError::MissingKey => (
            StatusCode::BAD_REQUEST,
            "This provider requires a key name from [ai.keys].".to_owned(),
        ),
        ModelListingError::MissingBaseUrl => (
            StatusCode::BAD_REQUEST,
            "Base URL is required for this provider.".to_owned(),
        ),
        ModelListingError::Unsupported => (
            StatusCode::BAD_REQUEST,
            "This provider does not support model listing.".to_owned(),
        ),
        ModelListingError::InvalidRequest => (
            StatusCode::BAD_REQUEST,
            "Could not build the provider request. Check Base URL and Key name.".to_owned(),
        ),
        ModelListingError::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "Model listing timed out. Increase Timeout or check provider load.".to_owned(),
        ),
        ModelListingError::Connect => (
            StatusCode::BAD_GATEWAY,
            "Could not connect to the provider. Check Base URL, DNS, and TLS.".to_owned(),
        ),
        ModelListingError::Transport => (
            StatusCode::BAD_GATEWAY,
            "Could not retrieve the model list. Check Base URL and network access.".to_owned(),
        ),
        ModelListingError::HttpStatus(status) => {
            let message = match status {
                401 | 403 => format!(
                    "Authentication failed (HTTP {status}). Check the selected [ai.keys] entry and its provider permissions."
                ),
                404 => format!(
                    "Model listing endpoint not found (HTTP {status}). Check Base URL or provider support."
                ),
                429 => {
                    "Provider rate limited model listing (HTTP 429). Try again later.".to_owned()
                }
                500..=599 => format!(
                    "Provider failed to list models (HTTP {status}). Check the provider's status and try again."
                ),
                _ => format!("Provider rejected model listing (HTTP {status})."),
            };
            (StatusCode::BAD_GATEWAY, message)
        }
        ModelListingError::MalformedResponse => (
            StatusCode::BAD_GATEWAY,
            "Provider returned an invalid model list.".to_owned(),
        ),
        ModelListingError::ResponseTooLarge => (
            StatusCode::BAD_GATEWAY,
            "Provider returned a model list larger than 8 MiB.".to_owned(),
        ),
    }
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
            return render_validation(&state, &session, vec![e], Some(name), Some(submitted));
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
            let rewrite_emotes = current.ai.emotes_agent.profile == target;
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
            if rewrite_emotes {
                o.ai.emotes_agent.profile = Some(Some(renamed.clone()));
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
            &state,
            &session,
            vec![FieldError {
                field: "ai.profiles".into(),
                message: format!("Can't delete {name:?}: at least one profile is required."),
            }],
            None,
            None,
        );
    }
    let assigned = assignments(&name, &settings);
    if !assigned.is_empty() {
        return render_validation(
            &state,
            &session,
            vec![FieldError {
                field: "ai.profiles".into(),
                message: format!(
                    "Can't delete {name:?}: it is still assigned to {}. Assign them to another profile in Settings first.",
                    assignment_names(&assigned)
                ),
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
            render_validation(state, session, errs, edit_on_error, submitted)
        }
        Err(e) => Err(WebError::Internal(eyre::eyre!("settings apply: {e}"))),
    }
}

fn render_validation(
    state: &WebState,
    session: &Session,
    errs: Vec<FieldError>,
    edit_on_error: Option<String>,
    submitted: Option<FormState>,
) -> Result<Response, WebError> {
    render_page(state, session, errs, edit_on_error, submitted, None)
}

fn render_page(
    state: &WebState,
    session: &Session,
    errs: Vec<FieldError>,
    edit_on_error: Option<String>,
    submitted: Option<FormState>,
    connection_test: Option<ConnectionTestView>,
) -> Result<Response, WebError> {
    let settings = state.settings.load();
    let rows = settings.ai.profiles.clone();
    let submitted_row = submitted.as_ref().map(|form| SubmittedRow {
        form,
        // Edits replace the target row in place; creates append one past
        // the persisted rows (see the `create` / `update` mutators).
        index: match &edit_on_error {
            Some(target) => rows.iter().position(|p| p.name == *target),
            None => Some(rows.len()),
        },
    });
    let PageErrors {
        form: form_errors,
        global: global_errors,
    } = classify_errors(errs, &rows, submitted_row.as_ref());
    let total = rows.len();
    let cards: Vec<CardView> = rows.iter().map(|p| card_view(p, &settings)).collect();

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
        form_errors,
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

#[cfg(test)]
mod tests {
    use super::*;
    use twitch_1337_core::settings::ModelListing;

    #[test]
    fn form_omits_load_models_for_an_unsupported_capability() {
        let caps = ProviderCaps {
            efforts: Efforts::None,
            service_tier: false,
            default_base_url: None,
            model_listing: ModelListing::None,
        };
        let tpl = ListTpl {
            rows: Vec::new(),
            edit_form: None,
            edit_target: None,
            new_form: Some(FormState {
                provider: AiProvider::Ollama.as_str().to_owned(),
                ..FormState::new_default()
            }),
            form_errors: FormErrors::default(),
            global_errors: Vec::new(),
            flash: None,
            connection_test: None,
            csrf: String::new(),
            user_login: "owner".to_owned(),
            user_avatar_url: None,
            current_page: crate::nav::AI_PROFILES,
            is_mod: true,
            is_broadcaster: true,
            is_owner: true,
            total: 0,
            providers: vec![provider_option_with_caps(AiProvider::Ollama, caps)],
            kinds: ProfileKind::ALL,
        };

        let html = tpl.render().unwrap();
        assert!(!html.contains("Load models"), "{html}");
        assert!(
            html.contains("list=\"profile-new-model-options\""),
            "{html}"
        );
    }
}
