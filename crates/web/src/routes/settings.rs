//! Owner-only settings page: live cooldowns + pings runtime knobs.
//!
//! The page reads `state.settings` for the current effective values and
//! `state.settings_store.defaults()` to show the compile-time fallbacks
//! beside each input. Saves go through `SettingsStore::apply`, which
//! validates, atomically persists, swaps the shared handle, and records an
//! audit entry. Reset clears one section (`cooldowns` or `pings`) back to
//! its defaults.

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tower_cookies::Cookies;
use twitch_1337_core::settings::overrides::{
    AiBehaviorOverrides, AiConnectionOverrides, AiDreamerOverrides, AiEmotesOverrides,
    AiHistoryOverrides, AiMediaOverrides, AiMemoryOverrides, AiPrefillOverrides, AiWebOverrides,
};
use twitch_1337_core::settings::{
    Actor, AiBackendKind, AiOverrides, AiSettings, Cooldowns, CooldownsOverrides, FieldError,
    PingsOverrides, PingsSettings, Settings, SettingsError, SettingsOverrides, SettingsSection,
};

use crate::auth::Role;
use crate::auth::csrf;
use crate::auth::session::Session;
use crate::error::WebError;
use crate::flash;
use crate::routes::{render, render_with};
use crate::state::WebState;

pub fn owner_router() -> Router<WebState> {
    Router::new()
        .route("/settings", get(show).post(save))
        .route("/settings/reset/{section}", post(reset))
}

#[derive(Template)]
#[template(path = "settings/index.html")]
struct ShowTpl {
    csrf: String,
    flash: Option<String>,
    user_login: String,
    user_avatar_url: Option<String>,
    current_page: &'static str,
    is_mod: bool,
    is_broadcaster: bool,
    is_owner: bool,
    current: Settings,
    defaults: Settings,
    errors: Vec<FieldError>,
}

async fn show(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
) -> Result<Response, WebError> {
    let current = (**state.settings.load()).clone();
    let defaults = state.settings_store.defaults().clone();
    render(&ShowTpl {
        csrf: csrf::encode(&session.csrf_value),
        flash: flash::take(&cookies),
        user_login: session.user_login.clone(),
        user_avatar_url: session.avatar_url.clone(),
        current_page: crate::nav::SETTINGS,
        is_mod: session.is_mod(),
        is_broadcaster: session.is_broadcaster,
        is_owner: matches!(session.role, Role::Owner),
        current,
        defaults,
        errors: Vec::new(),
    })
}

#[derive(Deserialize)]
struct SaveForm {
    #[serde(rename = "_csrf")]
    csrf: String,
    cooldown_ai: u64,
    cooldown_news: u64,
    cooldown_up: u64,
    cooldown_feedback: u64,
    cooldown_doener: u64,
    ping_cooldown: u64,
    /// Unchecked HTML checkboxes don't submit a value at all, so a missing
    /// `ping_public` key means "false". Form fields with `value="1"` send
    /// `Some("1")` when checked.
    #[serde(default)]
    ping_public: Option<String>,

    // ---- AI connection card ----
    #[serde(default)]
    ai_connection_backend: Option<String>,
    #[serde(default)]
    ai_connection_base_url: Option<String>,
    #[serde(default)]
    ai_connection_model: Option<String>,
    #[serde(default)]
    ai_connection_timeout: Option<u64>,
    #[serde(default)]
    ai_connection_reasoning_effort: Option<String>,

    // ---- AI behavior card ----
    #[serde(default)]
    ai_behavior_max_turn_rounds: Option<usize>,
    #[serde(default)]
    ai_behavior_max_writes_per_turn: Option<usize>,
    #[serde(default)]
    ai_behavior_persona_name: Option<String>,

    // ---- AI history card ----
    #[serde(default)]
    ai_history_length: Option<u64>,
    #[serde(default)]
    ai_history_ai_channel_length: Option<u64>,

    // ---- AI memory card ----
    #[serde(default)]
    ai_memory_soul_bytes: Option<usize>,
    #[serde(default)]
    ai_memory_lore_bytes: Option<usize>,
    #[serde(default)]
    ai_memory_user_bytes: Option<usize>,
    #[serde(default)]
    ai_memory_state_bytes: Option<usize>,
    #[serde(default)]
    ai_memory_inject_byte_budget: Option<usize>,
    #[serde(default)]
    ai_memory_max_state_files: Option<usize>,

    // ---- AI dreamer card ----
    #[serde(default)]
    ai_dreamer_enabled: Option<String>,
    #[serde(default)]
    ai_dreamer_model: Option<String>,
    #[serde(default)]
    ai_dreamer_reasoning_effort: Option<String>,
    #[serde(default)]
    ai_dreamer_run_at: Option<String>,
    #[serde(default)]
    ai_dreamer_timeout_secs: Option<u64>,
    #[serde(default)]
    ai_dreamer_max_rounds: Option<usize>,

    // ---- AI prefill toggle card ----
    /// Hidden marker emitted by the rendered card so the handler can tell
    /// "card visible but unchecked" (Some, None) from "card not in form"
    /// (None). Templates add this in Task 19.
    #[serde(default)]
    ai_prefill_card_visible: Option<String>,
    #[serde(default)]
    ai_prefill_enabled: Option<String>,
    #[serde(default)]
    ai_prefill_base_url: Option<String>,
    #[serde(default)]
    ai_prefill_threshold: Option<f64>,

    // ---- AI web toggle card ----
    #[serde(default)]
    ai_web_card_visible: Option<String>,
    #[serde(default)]
    ai_web_enabled: Option<String>,
    #[serde(default)]
    ai_web_base_url: Option<String>,
    #[serde(default)]
    ai_web_timeout: Option<u64>,
    #[serde(default)]
    ai_web_max_results: Option<usize>,
    #[serde(default)]
    ai_web_max_rounds: Option<usize>,
    #[serde(default)]
    ai_web_cache_ttl_secs: Option<u64>,
    #[serde(default)]
    ai_web_cache_capacity: Option<usize>,

    // ---- AI emotes toggle card ----
    #[serde(default)]
    ai_emotes_card_visible: Option<String>,
    #[serde(default)]
    ai_emotes_enabled: Option<String>,
    #[serde(default)]
    ai_emotes_include_global: Option<String>,
    #[serde(default)]
    ai_emotes_refresh_interval_secs: Option<u64>,
    #[serde(default)]
    ai_emotes_max_prompt_emotes: Option<usize>,
    #[serde(default)]
    ai_emotes_min_baseline_emotes: Option<usize>,
    #[serde(default)]
    ai_emotes_base_url: Option<String>,

    // ---- AI media card ----
    #[serde(default)]
    ai_media_model: Option<String>,
    #[serde(default)]
    ai_media_timeout: Option<u64>,
    #[serde(default)]
    ai_media_max_image_size: Option<String>,
    #[serde(default)]
    ai_media_max_pdf_size: Option<String>,
    #[serde(default)]
    ai_media_max_audio_size: Option<String>,
    #[serde(default)]
    ai_media_max_video_size: Option<String>,
    #[serde(default)]
    ai_media_max_text_size: Option<String>,
}

/// Map a submitted form into a sparse [`AiOverrides`] patch.
///
/// Every field absent from the form maps to `None` ("no change"); the helper
/// applies a few input-shape rules:
///
/// - `backend`: parses `"openai"`/`"ollama"`; unknown strings drop to `None`
///   so the validator can surface them.
/// - `base_url` (connection + emotes): empty string is an explicit clear
///   (`Some(None)`); non-empty is `Some(Some(_))`.
/// - `reasoning_effort` (connection + dreamer): the `"none"` sentinel from the
///   segmented selector clears the override; empty also clears.
/// - Toggle cards (prefill/web/emotes) use a `*_card_visible` hidden input
///   to differentiate "card rendered but unchecked" (`Some(false)`) from
///   "card not in form" (`None`).
/// - Media size caps parse via [`bytesize::ByteSize::from_str`]; malformed
///   strings are dropped (`None`) and the validator catches the lapse.
fn form_into_ai_overrides(form: &SaveForm) -> AiOverrides {
    fn enabled_from_card(visible: &Option<String>, checked: &Option<String>) -> Option<bool> {
        match (visible, checked) {
            (Some(_), Some(_)) => Some(true),
            (Some(_), None) => Some(false),
            (None, _) => None,
        }
    }

    let connection = AiConnectionOverrides {
        backend: form.ai_connection_backend.as_deref().and_then(|s| match s {
            "openai" => Some(AiBackendKind::OpenAi),
            "ollama" => Some(AiBackendKind::Ollama),
            _ => None,
        }),
        base_url: form.ai_connection_base_url.as_ref().map(|s| {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }),
        model: form.ai_connection_model.clone(),
        timeout: form.ai_connection_timeout,
        reasoning_effort: form.ai_connection_reasoning_effort.as_ref().map(|s| {
            if s == "none" || s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }),
    };

    let behavior = AiBehaviorOverrides {
        max_turn_rounds: form.ai_behavior_max_turn_rounds,
        max_writes_per_turn: form.ai_behavior_max_writes_per_turn,
        persona_name: form
            .ai_behavior_persona_name
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };

    let history = AiHistoryOverrides {
        length: form.ai_history_length,
        ai_channel_length: form.ai_history_ai_channel_length,
    };

    let memory = AiMemoryOverrides {
        soul_bytes: form.ai_memory_soul_bytes,
        lore_bytes: form.ai_memory_lore_bytes,
        user_bytes: form.ai_memory_user_bytes,
        state_bytes: form.ai_memory_state_bytes,
        inject_byte_budget: form.ai_memory_inject_byte_budget,
        max_state_files: form.ai_memory_max_state_files,
    };

    let dreamer = AiDreamerOverrides {
        // The dreamer card always renders, so checkbox semantics are the same
        // as `ping_public`: missing key = false, present = true.
        enabled: Some(form.ai_dreamer_enabled.is_some()),
        model: form
            .ai_dreamer_model
            .as_ref()
            .map(|v| if v.is_empty() { None } else { Some(v.clone()) }),
        reasoning_effort: form.ai_dreamer_reasoning_effort.as_ref().map(|v| {
            if v == "none" || v.is_empty() {
                None
            } else {
                Some(v.clone())
            }
        }),
        run_at: form.ai_dreamer_run_at.clone(),
        timeout_secs: form.ai_dreamer_timeout_secs,
        max_rounds: form.ai_dreamer_max_rounds,
    };

    let prefill = AiPrefillOverrides {
        enabled: enabled_from_card(&form.ai_prefill_card_visible, &form.ai_prefill_enabled),
        base_url: form.ai_prefill_base_url.clone(),
        threshold: form.ai_prefill_threshold,
    };

    let web = AiWebOverrides {
        enabled: enabled_from_card(&form.ai_web_card_visible, &form.ai_web_enabled),
        base_url: form.ai_web_base_url.clone(),
        timeout: form.ai_web_timeout,
        max_results: form.ai_web_max_results,
        max_rounds: form.ai_web_max_rounds,
        cache_ttl_secs: form.ai_web_cache_ttl_secs,
        cache_capacity: form.ai_web_cache_capacity,
    };

    let emotes = AiEmotesOverrides {
        enabled: enabled_from_card(&form.ai_emotes_card_visible, &form.ai_emotes_enabled),
        // include_global only meaningful when the card is visible; mirror
        // the prefill/web pattern so the inner checkbox can be unchecked
        // explicitly without disabling the whole card.
        include_global: form
            .ai_emotes_card_visible
            .as_ref()
            .map(|_| form.ai_emotes_include_global.is_some()),
        refresh_interval_secs: form.ai_emotes_refresh_interval_secs,
        max_prompt_emotes: form.ai_emotes_max_prompt_emotes,
        min_baseline_emotes: form.ai_emotes_min_baseline_emotes,
        base_url: form
            .ai_emotes_base_url
            .as_ref()
            .map(|v| if v.is_empty() { None } else { Some(v.clone()) }),
    };

    let media = AiMediaOverrides {
        model: form.ai_media_model.clone(),
        timeout: form.ai_media_timeout,
        max_image_size: form
            .ai_media_max_image_size
            .as_deref()
            .and_then(|s| s.parse().ok()),
        max_pdf_size: form
            .ai_media_max_pdf_size
            .as_deref()
            .and_then(|s| s.parse().ok()),
        max_audio_size: form
            .ai_media_max_audio_size
            .as_deref()
            .and_then(|s| s.parse().ok()),
        max_video_size: form
            .ai_media_max_video_size
            .as_deref()
            .and_then(|s| s.parse().ok()),
        max_text_size: form
            .ai_media_max_text_size
            .as_deref()
            .and_then(|s| s.parse().ok()),
    };

    AiOverrides {
        connection,
        behavior,
        history,
        memory,
        dreamer,
        prefill,
        web,
        emotes,
        media,
    }
}

/// Identify resolved AI fields whose change cannot take effect without a
/// process restart, so the dashboard can surface a "restart bot to apply"
/// hint in the flash. The rest of the AI knobs are read live via
/// `SettingsHandle` and require no restart.
fn restart_required(before: &AiSettings, after: &AiSettings) -> Vec<String> {
    let mut out = Vec::new();
    if before.connection.backend != after.connection.backend {
        out.push("ai.connection.backend".into());
    }
    if before.connection.base_url != after.connection.base_url {
        out.push("ai.connection.base_url".into());
    }
    if before.prefill.is_none() && after.prefill.is_some() {
        out.push("ai.prefill (enabling requires restart)".into());
    }
    if before.web.is_none() && after.web.is_some() {
        out.push("ai.web (enabling requires restart)".into());
    }
    if before.emotes.is_none() && after.emotes.is_some() {
        out.push("ai.emotes (enabling requires restart)".into());
    }
    if let (Some(a), Some(b)) = (&before.prefill, &after.prefill)
        && a.base_url != b.base_url
    {
        out.push("ai.prefill.base_url".into());
    }
    out
}

async fn save(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    axum::Form(form): axum::Form<SaveForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }

    let before = (**state.settings.load()).clone();

    let patch = SettingsOverrides {
        schema_version: twitch_1337_core::settings::SCHEMA_VERSION,
        cooldowns: CooldownsOverrides {
            ai: Some(form.cooldown_ai),
            news: Some(form.cooldown_news),
            up: Some(form.cooldown_up),
            feedback: Some(form.cooldown_feedback),
            doener: Some(form.cooldown_doener),
        },
        pings: PingsOverrides {
            cooldown: Some(form.ping_cooldown),
            public: Some(form.ping_public.is_some()),
        },
        ai: form_into_ai_overrides(&form),
    };

    let actor = Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    };

    match state.settings_store.apply(patch, actor).await {
        Ok(after) => {
            let restart = restart_required(&before.ai, &after.ai);
            tracing::info!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "settings_apply",
                result = "ok",
                restart_required = restart.len(),
            );
            let flash_msg = if restart.is_empty() {
                "Settings saved.".to_string()
            } else {
                format!(
                    "Settings saved. Restart bot to apply: {}",
                    restart.join(", ")
                )
            };
            flash::set(&cookies, &flash_msg);
            Ok(Redirect::to("/settings").into_response())
        }
        Err(SettingsError::Validation(errors)) => {
            tracing::info!(
                target: "twitch_1337_web",
                user_id = %session.user_id,
                action = "settings_apply",
                result = "validation",
                error_count = errors.len(),
            );
            // Preserve the user's submitted (raw) values so they can correct
            // the invalid field without having to retype every other input.
            // Spec §7.2: "previously entered values are preserved".
            let submitted = Settings {
                schema_version: twitch_1337_core::settings::SCHEMA_VERSION,
                cooldowns: Cooldowns {
                    ai: form.cooldown_ai,
                    news: form.cooldown_news,
                    up: form.cooldown_up,
                    feedback: form.cooldown_feedback,
                    doener: form.cooldown_doener,
                },
                pings: PingsSettings {
                    cooldown: form.ping_cooldown,
                    public: form.ping_public.is_some(),
                },
                // AI submitted-value preservation is left as a follow-up:
                // it requires the per-card templates (Task 19) to read echoed
                // values rather than the live `current.ai`. Until then, the
                // re-render falls back to compiled defaults for the AI section.
                ai: AiSettings::default(),
            };
            let defaults = state.settings_store.defaults().clone();
            render_with(
                axum::http::StatusCode::BAD_REQUEST,
                &ShowTpl {
                    csrf: csrf::encode(&session.csrf_value),
                    flash: None,
                    user_login: session.user_login.clone(),
                    user_avatar_url: session.avatar_url.clone(),
                    current_page: crate::nav::SETTINGS,
                    is_mod: session.is_mod(),
                    is_broadcaster: session.is_broadcaster,
                    is_owner: matches!(session.role, Role::Owner),
                    current: submitted,
                    defaults,
                    errors,
                },
            )
        }
        Err(e) => Err(WebError::Internal(eyre::eyre!("settings apply: {e}"))),
    }
}

#[derive(Deserialize)]
struct ResetForm {
    #[serde(rename = "_csrf")]
    csrf: String,
}

async fn reset(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    cookies: Cookies,
    Path(section): Path<String>,
    axum::Form(form): axum::Form<ResetForm>,
) -> Result<Response, WebError> {
    if !csrf::verify(&form.csrf, &session.csrf_value) {
        return Err(WebError::CsrfMismatch);
    }
    let section = match section.as_str() {
        "cooldowns" => SettingsSection::Cooldowns,
        "pings" => SettingsSection::Pings,
        other => {
            return Err(WebError::Validation {
                field: "section".into(),
                msg: format!("unknown section `{other}`"),
            });
        }
    };
    let actor = Actor {
        user_id: session.user_id.clone(),
        user_login: session.user_login.clone(),
    };
    state.settings_store.reset(section, actor).await?;
    tracing::info!(
        target: "twitch_1337_web",
        user_id = %session.user_id,
        action = "settings_reset",
        section = ?section,
        result = "ok",
    );
    flash::set(&cookies, "Reset to defaults.");
    Ok(Redirect::to("/settings").into_response())
}
