//! Integration tests for the owner-only `/settings` page.
//!
//! Drives `build_router` so the owner middleware + handler chain are
//! exercised end-to-end. The fixture seeds the session table with an
//! `owner_id` that matches the inserted Owner session so the periodic
//! role recheck inside `require_owner` admits cleanly.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, Response, StatusCode, header};
use tempfile::TempDir;
use tower::ServiceExt as _;
use twitch_1337_core::settings::{
    Actor, AiOverrides, AiProfile, AiProvider, ProfileKind, Settings,
    overrides::{
        AiAuroraOverrides, AiDreamerOverrides, AiGrokOverrides, AiMediaOverrides, SettingsOverrides,
    },
};
use twitch_1337_web::WebState;
use twitch_1337_web::auth::Role;
use twitch_1337_web::build_router;

use super::helpers;
use helpers::{
    FakeHelix, build_state_with_all_dirs, cookie_header, insert_session_as, install_crypto,
    set_owner,
};

const OWNER_ID: &str = "123";

/// Fresh state owned by [`OWNER_ID`]; keep the tempdirs alive for the test.
async fn owner_state() -> (WebState, (TempDir, TempDir, TempDir)) {
    install_crypto();
    let helix = Arc::new(FakeHelix {
        moderators: vec![],
        users: Default::default(),
    });
    let (state, td_p, td_m, td_s) = build_state_with_all_dirs(helix).await;
    set_owner(&state, Some(OWNER_ID));
    (state, (td_p, td_m, td_s))
}

/// Signed cookies and bare CSRF token of an owner session.
struct Owner {
    sid: String,
    csrf_cookie: String,
    csrf: String,
}

impl Owner {
    fn login(state: &WebState) -> Self {
        let (sid, csrf_cookie, csrf) = insert_session_as(state, OWNER_ID, "owner", Role::Owner);
        Self {
            sid,
            csrf_cookie,
            csrf,
        }
    }

    async fn get(&self, state: &WebState, uri: &str) -> Response<Body> {
        let req = Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie_header(&self.sid, &self.csrf_cookie))
            .body(Body::empty())
            .unwrap();
        build_router(state.clone()).oneshot(req).await.unwrap()
    }

    async fn post(&self, state: &WebState, uri: &str, body: String) -> Response<Body> {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::COOKIE, cookie_header(&self.sid, &self.csrf_cookie))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        build_router(state.clone()).oneshot(req).await.unwrap()
    }

    /// POST `/settings` with every field at its `base` value plus `extra`.
    async fn save(
        &self,
        state: &WebState,
        base: &Settings,
        extra: &[(&str, &str)],
    ) -> Response<Body> {
        let body = save_form_body(&self.csrf, base, extra);
        self.post(state, "/settings", body).await
    }
}

/// POST `/settings` as a fresh owner session, based on the live settings.
async fn post_settings(state: &WebState, extra: &[(&str, &str)]) -> Response<Body> {
    let current = state.settings.load();
    Owner::login(state).save(state, &current, extra).await
}

async fn body_string(res: Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Return the inner HTML of the first `<select name="…">` with the given name.
fn select_inner<'a>(html: &'a str, name: &str) -> &'a str {
    let marker = format!("name=\"{name}\"");
    let after_name = html
        .split_once(&marker)
        .unwrap_or_else(|| panic!("missing <select {marker}>"))
        .1;
    let after_open = after_name
        .split_once('>')
        .unwrap_or_else(|| panic!("malformed <select {marker}>"))
        .1;
    after_open
        .split_once("</select>")
        .unwrap_or_else(|| panic!("unclosed <select {marker}>"))
        .0
}

/// Build a form body containing every required field at its current value
/// plus any extra (form_field, value) pairs the test wants to submit.
fn save_form_body(bare_csrf: &str, current: &Settings, extra: &[(&str, &str)]) -> String {
    let mut fields = vec![
        ("_csrf".to_owned(), bare_csrf.to_owned()),
        ("cooldown_ai".to_owned(), current.cooldowns.ai.to_string()),
        (
            "cooldown_news".to_owned(),
            current.cooldowns.news.to_string(),
        ),
        ("cooldown_up".to_owned(), current.cooldowns.up.to_string()),
        (
            "cooldown_feedback".to_owned(),
            current.cooldowns.feedback.to_string(),
        ),
        (
            "cooldown_doener".to_owned(),
            current.cooldowns.doener.to_string(),
        ),
        (
            "ping_cooldown".to_owned(),
            current.pings.cooldown.to_string(),
        ),
        (
            "ai_aurora_profile".to_owned(),
            current.ai.aurora.profile.clone(),
        ),
        (
            "ai_grok_profile".to_owned(),
            current.ai.grok.profile.clone(),
        ),
        (
            "ai_dreamer_profile".to_owned(),
            current.ai.dreamer.profile.clone(),
        ),
        (
            "ai_media_profile".to_owned(),
            current.ai.media.profile.clone(),
        ),
    ];
    for &(key, value) in extra {
        if let Some((_, current_value)) = fields.iter_mut().find(|(name, _)| name == key) {
            *current_value = value.to_owned();
        } else {
            fields.push((key.to_owned(), value.to_owned()));
        }
    }
    fields
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencoding::encode(&key),
                urlencoding::encode(&value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Pluck the `tw1337_flash` cookie value out of `Set-Cookie` headers.
/// Returns `None` if no flash cookie was emitted on the response.
fn read_flash(res: &Response<Body>) -> Option<String> {
    for c in res.headers().get_all(header::SET_COOKIE) {
        let s = c.to_str().ok()?;
        if let Some(rest) = s.strip_prefix("tw1337_flash=") {
            let value = rest.split(';').next()?;
            return Some(
                urlencoding::decode(value)
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_else(|_| value.to_owned()),
            );
        }
    }
    None
}

fn test_actor() -> Actor {
    Actor {
        user_id: "test".into(),
        user_login: "test".into(),
    }
}

fn chat_profile(name: &str) -> AiProfile {
    AiProfile {
        name: name.into(),
        key: Some("k".into()),
        provider: AiProvider::OpenRouter,
        base_url: None,
        model: "some-model".into(),
        timeout: 30,
        reasoning_effort: None,
        service_tier: None,
        ..Default::default()
    }
}

/// Persist chat profiles `names`, with `a` active and backing Aurora,
/// Dreamer and Media; Grok points at `grok_profile`.
async fn seed_chat_profiles(state: &WebState, names: &[&str], grok_profile: &str) {
    state
        .settings_store
        .apply(
            SettingsOverrides {
                ai: AiOverrides {
                    profiles: Some(names.iter().copied().map(chat_profile).collect()),
                    aurora: AiAuroraOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    grok: AiGrokOverrides {
                        profile: Some(Some(grok_profile.into())),
                        ..Default::default()
                    },
                    dreamer: AiDreamerOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    media: AiMediaOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            test_actor(),
        )
        .await
        .expect("seed profiles");
}

#[tokio::test]
async fn owner_can_save_cooldown_and_handle_reflects_change() {
    let (state, _dirs) = owner_state().await;
    // Construct a fully-populated form. The save handler treats every field
    // as authoritative, so we send the current value for everything except
    // the one knob we're changing.
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(&state, &defaults, &[("cooldown_ai", "15")])
        .await;
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "owner save should redirect (303)",
    );
    assert_eq!(
        res.headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/settings"),
    );
    assert_eq!(
        state.settings.load().cooldowns.ai,
        15,
        "live handle must reflect saved cooldown",
    );
}

#[tokio::test]
async fn non_owner_get_settings_returns_403() {
    let (state, _dirs) = owner_state().await;
    // Owner is someone else; the mod session must NOT be admitted as owner.
    set_owner(&state, Some("999"));
    let (sid, csrf_cookie, _bare) = insert_session_as(&state, "42", "modder", Role::Mod);

    let app = build_router(state);
    let req = Request::builder()
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "Mod session must not satisfy require_owner on /settings",
    );
}

#[tokio::test]
async fn validation_error_renders_form_with_errors() {
    let (state, _dirs) = owner_state().await;
    let before = state.settings.load().cooldowns.ai;
    let defaults = state.settings_store.defaults().clone();
    // The submitted news value (900) is valid and *different* from the
    // default (60), so the re-rendered form must echo it back to verify
    // we don't discard the user's other typed input on a validation error.
    assert_ne!(
        defaults.cooldowns.news, 900,
        "fixture assumes the default news cooldown is not already 900",
    );
    // cooldown_ai = 0 violates the 1..=3600 bound.
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[("cooldown_ai", "0"), ("cooldown_news", "900")],
        )
        .await;
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "validation failure must render form with 400",
    );
    let html = body_string(res).await;
    assert!(
        html.contains("cooldowns.ai"),
        "response must surface the failing field; got: {html}"
    );
    // Submitted-value preservation: the valid sibling field (news=900) must
    // be echoed back into the form, not replaced by the stored default.
    assert!(
        html.contains("value=\"900\""),
        "submitted cooldown_news must be preserved on validation error; got: {html}"
    );
    assert_eq!(
        state.settings.load().cooldowns.ai,
        before,
        "validation rejection must not mutate the live handle",
    );
}

/// A required Dreamer profile round-trips and cannot be blank.
#[tokio::test]
async fn a_required_dreamer_profile_round_trips_through_the_settings_post() {
    let (state, _dirs) = owner_state().await;
    seed_chat_profiles(&state, &["default", "a", "b"], "a").await;

    let owner = Owner::login(&state);
    let current = state.settings.load();
    let res = owner
        .save(&state, &current, &[("ai_dreamer_profile", "b")])
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        state.settings.load().ai.dreamer.profile,
        "b",
        "dreamer.profile must be set to the submitted profile name",
    );

    let current = state.settings.load();
    let res = owner
        .save(&state, &current, &[("ai_dreamer_profile", "")])
        .await;
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "blank required Dreamer profile must fail validation even when `default` exists",
    );
    let html = body_string(res).await;
    assert!(
        html.contains("ai.dreamer.profile") && html.contains("must not be empty"),
        "response must surface the required Dreamer profile error; got: {html}",
    );
    assert_eq!(
        state.settings.load().ai.dreamer.profile,
        "b",
        "validation rejection must not silently switch Dreamer to `default`",
    );
}

/// Recap's `recap_profile` round-trips through the settings POST.
/// Empty = Recap off (never inherit the active chat profile).
#[tokio::test]
async fn a_recap_profile_round_trips_through_the_settings_post() {
    let (state, _dirs) = owner_state().await;
    seed_chat_profiles(&state, &["a", "b"], "a").await;

    let owner = Owner::login(&state);
    let current = state.settings.load();
    let res = owner
        .save(
            &state,
            &current,
            &[
                ("ai_recap_profile", "b"),
                ("ai_recap_batch_size", "30"),
                ("ai_recap_max_notes", "250"),
                ("ai_recap_merge_width", "25"),
                // Legacy History recap fields must be ignored by schema-7 saves.
                ("ai_history_batch_size", "1"),
                ("ai_history_max_notes", "1"),
                ("ai_history_merge_width", "1"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        state.settings.load().ai.recap.profile.as_deref(),
        Some("b"),
        "recap.profile must be set to the submitted profile name",
    );
    assert_eq!(state.settings.load().ai.recap.batch_size, 30);
    assert_eq!(state.settings.load().ai.recap.max_notes, 250);
    assert_eq!(state.settings.load().ai.recap.merge_width, 25);

    let current = state.settings.load();
    let res = owner
        .save(&state, &current, &[("ai_recap_profile", "")])
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        state.settings.load().ai.recap.profile,
        None,
        "an empty submission must turn Recap off, not inherit the active profile",
    );
}

#[tokio::test]
async fn grok_profile_and_persona_round_trip_through_the_settings_post() {
    let (state, _dirs) = owner_state().await;
    let current = state.settings.load();
    let res = Owner::login(&state)
        .save(
            &state,
            &current,
            &[
                ("ai_grok_profile", current.ai.grok.profile.as_str()),
                ("ai_grok_persona_name", "Grok Prime"),
            ],
        )
        .await;

    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().ai.grok.persona_name, "Grok Prime");
}

#[tokio::test]
async fn post_settings_enabling_prefill_card_flags_restart() {
    let (state, _dirs) = owner_state().await;
    // Prefill is disabled by default; rendering the toggle card and ticking
    // the box enables it — that transition requires a restart.
    assert!(
        state.settings.load().ai.prefill.is_none(),
        "fixture assumes prefill defaults to disabled",
    );

    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[
                ("ai_prefill_card_visible", "1"),
                ("ai_prefill_enabled", "1"),
                ("ai_prefill_base_url", "https://logs.zonian.dev"),
                ("ai_prefill_threshold", "0.5"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let flash = read_flash(&res).expect("save must emit a flash cookie");
    assert!(
        flash.contains("ai.prefill"),
        "enabling prefill must mention the field in the restart hint; got {flash:?}",
    );
    assert!(
        state.settings.load().ai.prefill.is_some(),
        "prefill must be enabled after apply",
    );
}

#[tokio::test]
async fn post_settings_card_invisible_leaves_toggle_alone() {
    // Regression for the *_card_visible hidden-input pattern: a save that
    // does not render a toggle card must NOT inadvertently disable it.
    let (state, _dirs) = owner_state().await;
    let owner = Owner::login(&state);

    // First, enable prefill via a "card visible" save so subsequent saves
    // without the hidden input must preserve that state.
    let defaults = state.settings_store.defaults().clone();
    let res = owner
        .save(
            &state,
            &defaults,
            &[
                ("ai_prefill_card_visible", "1"),
                ("ai_prefill_enabled", "1"),
                ("ai_prefill_base_url", "https://logs.zonian.dev"),
                ("ai_prefill_threshold", "0.5"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(state.settings.load().ai.prefill.is_some());

    // Now save again without rendering the card at all — prefill must stay on.
    let res = owner.save(&state, &defaults, &[]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        state.settings.load().ai.prefill.is_some(),
        "a save without the card_visible marker must NOT clear an enabled card",
    );
}

#[tokio::test]
async fn owner_can_save_aurora_reply_length() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[
                ("ai_aurora_reply_target_chars", "200"),
                ("ai_aurora_reply_soft_chars", "300"),
                ("ai_aurora_reply_hard_chars", "400"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let snap = state.settings.load();
    assert_eq!(snap.ai.aurora.reply_target_chars, 200);
    assert_eq!(snap.ai.aurora.reply_soft_chars, 300);
    assert_eq!(snap.ai.aurora.reply_hard_chars, 400);
}

#[tokio::test]
async fn owner_can_save_ai_history_n() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(&state, &defaults, &[("ai_history_n", "25")])
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().ai.history.n, 25);
}

#[tokio::test]
async fn ai_history_length_below_n_plus_batch_returns_400() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    assert_eq!(defaults.ai.history.n, 20);
    assert_eq!(defaults.ai.recap.batch_size, 40);

    let res = Owner::login(&state)
        .save(&state, &defaults, &[("ai_history_length", "10")])
        .await;
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "length < n + batch_size must fail validation",
    );
    let html = body_string(res).await;
    assert!(
        html.contains("ai.history.length"),
        "response must surface the failing field; got: {html}",
    );
}

#[tokio::test]

async fn permissions_lists_render_saved_values() {
    // hidden_admins / viewer_allowlist / emote_usage_deny_list are the
    // multi-line (textarea) rows: one entry per line, posted back verbatim.
    let (state, _dirs) = owner_state().await;
    let owner = Owner::login(&state);

    // Seed the permission lists via POST.
    let defaults = state.settings_store.defaults().clone();
    let res = owner
        .save(
            &state,
            &defaults,
            &[
                ("twitch_hidden_admins", "111\n222"),
                ("twitch_viewer_allowlist", "333\n444"),
                ("twitch_emote_usage_deny_list", "evilbot\notherbot"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let s = state.settings.load();
    assert_eq!(
        s.twitch.hidden_admins,
        vec!["111".to_owned(), "222".to_owned()],
        "hidden_admins must persist from POST",
    );
    assert_eq!(
        s.twitch.emote_usage_deny_list,
        vec!["evilbot".to_owned(), "otherbot".to_owned()],
        "emote_usage_deny_list must persist from POST"
    );

    // GET and confirm each textarea renders its saved entries one per line.
    let res = owner.get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    for (name, joined) in [
        ("twitch_hidden_admins", "111\n222"),
        ("twitch_viewer_allowlist", "333\n444"),
        ("twitch_emote_usage_deny_list", "evilbot\notherbot"),
    ] {
        let start = html
            .find(&format!("name=\"{name}\""))
            .unwrap_or_else(|| panic!("{name} textarea missing"));
        let body = &html[start..];
        let body = &body[body.find('>').unwrap() + 1..body.find("</textarea>").unwrap()];
        assert_eq!(body, joined, "{name} must render its saved entries");
    }
}

#[tokio::test]

async fn dreamer_checkbox_absent_sets_enabled_false() {
    // The dreamer card always renders (no *_card_visible pattern), so
    // omitting ai_dreamer_enabled from the POST means "disabled".
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    // Send the form without ai_dreamer_enabled.
    let res = Owner::login(&state).save(&state, &defaults, &[]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        !state.settings.load().ai.dreamer.enabled,
        "missing ai_dreamer_enabled checkbox must set dreamer.enabled = false",
    );
}

#[tokio::test]
async fn emotes_card_include_global_absent_without_card_visible() {
    // When ai_emotes_card_visible is not submitted (card not rendered),
    // include_global override must be None — i.e. no change to the existing value.
    let (state, _dirs) = owner_state().await;
    let owner = Owner::login(&state);

    // First, enable emotes so there's an existing include_global value to preserve.
    let defaults = state.settings_store.defaults().clone();
    let res = owner
        .save(
            &state,
            &defaults,
            &[
                ("ai_emotes_card_visible", "1"),
                ("ai_emotes_enabled", "1"),
                ("ai_emotes_include_global", "1"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let include_global_before = state
        .settings
        .load()
        .ai
        .emotes
        .as_ref()
        .map(|e| e.include_global);
    assert_eq!(include_global_before, Some(true));

    // Now submit a form without the card_visible marker: include_global must stay.
    let res = owner.save(&state, &defaults, &[]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let include_global_after = state
        .settings
        .load()
        .ai
        .emotes
        .as_ref()
        .map(|e| e.include_global);
    assert_eq!(
        include_global_after,
        Some(true),
        "include_global must not change when card_visible was absent",
    );
}

#[tokio::test]
async fn emotes_card_pinned_emotes_persists_from_textarea() {
    // Pinned emotes (one-per-line textarea) must round-trip through the save
    // handler into the live settings handle.
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[
                ("ai_emotes_card_visible", "1"),
                ("ai_emotes_enabled", "1"),
                ("ai_emotes_max_prompt_emotes", "20"),
                ("ai_emotes_pinned_emotes", "PepeLa\nokjj\nClueless"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let pinned = state
        .settings
        .load()
        .ai
        .emotes
        .as_ref()
        .map(|e| e.pinned_emotes.clone());
    assert_eq!(
        pinned,
        Some(vec!["PepeLa".into(), "okjj".into(), "Clueless".into()]),
        "pinned_emotes must persist from the textarea",
    );
}

#[tokio::test]
async fn twitch_channels_save_persists() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(&state, &defaults, &[("twitch_expected_latency", "200")])
        .await;
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "twitch_channels save must redirect"
    );
    assert_eq!(
        state.settings.load().twitch.expected_latency,
        200,
        "expected_latency must persist",
    );
}

#[tokio::test]
async fn twitch_expected_latency_out_of_range_returns_400() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    // 1500 exceeds the 0..=1000 bound.
    let res = Owner::login(&state)
        .save(&state, &defaults, &[("twitch_expected_latency", "1500")])
        .await;
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "out-of-range expected_latency must return 400",
    );
}

#[tokio::test]
async fn aviationstack_save_persists() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[
                ("aviationstack_card_visible", "1"),
                ("aviationstack_enabled", "1"),
                ("aviationstack_base_url", "https://api.example.com/v1"),
                ("aviationstack_timeout_secs", "10"),
            ],
        )
        .await;
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "aviationstack save must redirect"
    );
    let s = state.settings.load();
    assert!(s.aviationstack.enabled, "enabled must persist");
    assert_eq!(
        s.aviationstack.base_url, "https://api.example.com/v1",
        "base_url must persist",
    );
    assert_eq!(
        s.aviationstack.timeout_secs, 10,
        "timeout_secs must persist"
    );
}

#[tokio::test]
async fn suspend_save_persists() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[("suspend_default_duration_secs", "1800")],
        )
        .await;
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "suspend save must redirect"
    );
    assert_eq!(
        state.settings.load().suspend.default_duration_secs,
        1800,
        "default_duration_secs must persist",
    );
}

#[tokio::test]
async fn web_runtime_save_persists() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[
                ("web_session_ttl_secs", "14400"),
                ("web_mod_check_refresh_secs", "60"),
            ],
        )
        .await;
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "web_runtime save must redirect"
    );
    let s = state.settings.load();
    assert_eq!(
        s.web.session_ttl_secs, 14400,
        "session_ttl_secs must persist"
    );
    assert_eq!(
        s.web.mod_check_refresh_secs, 60,
        "mod_check_refresh_secs must persist"
    );
}

/// A validation error must re-render untouched cards from the *stored*
/// settings, not from compiled defaults: the whole page posts as one form,
/// so a card echoed at its default gets submitted as an explicit
/// "set to default" on the corrective save and wipes settings silently.
#[tokio::test]
async fn validation_error_preserves_stored_ai_settings() {
    let (state, _dirs) = owner_state().await;
    seed_chat_profiles(&state, &["a", "b"], "b").await;

    let current = state.settings.load();
    // Fail validation on a different card: the inject budget must cover
    // SOUL + LORE, and 1024 bytes does not.
    let res = Owner::login(&state)
        .save(
            &state,
            &current,
            &[("ai_memory_inject_byte_budget", "1024")],
        )
        .await;
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "too-small inject budget must fail validation",
    );
    let html = body_string(res).await;
    assert!(
        html.contains("inject_byte_budget"),
        "response must surface the failing field; got: {html}",
    );
    assert!(
        html.contains("name=\"ai_grok_profile\"") && html.contains("value=\"b\" selected"),
        "stored grok profile override must survive the validation re-render; got: {html}",
    );
    assert_eq!(
        state.settings.load().ai.grok.profile,
        "b",
        "validation rejection must not mutate stored settings",
    );
}

fn decision_profile(name: &str) -> AiProfile {
    AiProfile {
        name: name.into(),
        key: Some("k".into()),
        provider: AiProvider::OpenRouter,
        model: "decider".into(),
        timeout: 30,
        kind: ProfileKind::Decision,
        ..Default::default()
    }
}

/// Adds a Decision profile named `dec` to the persisted profile list and
/// optionally points `spoiler.profile` at it.
async fn seed_decision_profile(state: &WebState, point_spoiler: bool) {
    let mut profiles = state.settings.load().ai.profiles.clone();
    profiles.push(decision_profile("dec"));
    let mut o = SettingsOverrides::default();
    o.ai.profiles = Some(profiles);
    if point_spoiler {
        o.spoiler.profile = Some("dec".into());
        o.spoiler.default_game = Some("Hades 2".into());
        o.spoiler.confidence_floor = Some(70);
    }
    state
        .settings_store
        .apply(o, test_actor())
        .await
        .expect("seed decision profile");
}

#[tokio::test]
async fn spoiler_card_save_persists() {
    let (state, _dirs) = owner_state().await;
    seed_decision_profile(&state, false).await;
    let res = post_settings(
        &state,
        &[
            ("spoiler_profile", "dec"),
            ("spoiler_default_game", "Elden Ring"),
            ("spoiler_confidence_floor", "65"),
            ("cooldown_spoiler", "45"),
            ("cooldown_servers", "50"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let s = state.settings.load();
    assert_eq!(s.spoiler.profile, "dec");
    assert_eq!(s.spoiler.default_game, "Elden Ring");
    assert_eq!(s.spoiler.confidence_floor, 65);
    assert_eq!(s.cooldowns.spoiler, 45);
    assert_eq!(s.cooldowns.servers, 50);
}

#[tokio::test]
async fn empty_spoiler_profile_persists_as_off() {
    let (state, _dirs) = owner_state().await;
    seed_decision_profile(&state, true).await;
    assert_eq!(state.settings.load().spoiler.profile, "dec");
    let res = post_settings(
        &state,
        &[
            ("spoiler_profile", ""),
            ("spoiler_default_game", "Hades 2"),
            ("spoiler_confidence_floor", "70"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().spoiler.profile, "");
}

/// The real page always posts the spoiler card, but a form that omits the
/// fields (older tab, hand-rolled client) must leave persisted values alone.
#[tokio::test]
async fn save_without_spoiler_fields_keeps_persisted_spoiler_settings() {
    let (state, _dirs) = owner_state().await;
    seed_decision_profile(&state, true).await;
    let res = post_settings(&state, &[("cooldown_ai", "99")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let s = state.settings.load();
    assert_eq!(s.cooldowns.ai, 99);
    assert_eq!(s.spoiler.profile, "dec");
    assert_eq!(s.spoiler.default_game, "Hades 2");
    assert_eq!(s.spoiler.confidence_floor, 70);
}

#[tokio::test]
async fn posted_spoiler_fields_round_trip_unchanged() {
    let (state, _dirs) = owner_state().await;
    seed_decision_profile(&state, true).await;
    let before = state.settings.load().spoiler.clone();
    let res = post_settings(
        &state,
        &[
            ("spoiler_profile", "dec"),
            ("spoiler_default_game", "Hades 2"),
            ("spoiler_confidence_floor", "70"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().spoiler, before);
}

/// Each profile picker offers only the profiles its agent may use, and an
/// empty "off" option only where the agent is optional.
#[tokio::test]
async fn profile_pickers_list_only_eligible_profiles() {
    let (state, _dirs) = owner_state().await;
    seed_decision_profile(&state, false).await;
    let res = Owner::login(&state).get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("id=\"sec-spoiler\""), "spoiler card missing");
    assert!(html.contains("name=\"cooldown_spoiler\""));
    assert!(html.contains("name=\"cooldown_servers\""));
    let inner = select_inner(&html, "spoiler_profile");
    assert!(
        inner.contains("value=\"\""),
        "needs the off option: {inner}"
    );
    assert!(inner.contains("value=\"dec\""), "{inner}");
    let chat_names: Vec<String> = state
        .settings
        .load()
        .ai
        .profiles
        .iter()
        .filter(|p| p.kind == ProfileKind::Chat)
        .map(|p| p.name.clone())
        .collect();
    assert!(!chat_names.is_empty());
    for n in &chat_names {
        assert!(
            !inner.contains(&format!("value=\"{n}\"")),
            "chat profile {n} leaked into spoiler dropdown"
        );
    }
    for agent in [
        "ai_aurora_profile",
        "ai_grok_profile",
        "ai_dreamer_profile",
        "ai_media_profile",
        "ai_recap_profile",
    ] {
        let inner = select_inner(&html, agent);
        assert!(
            !inner.contains("value=\"dec\""),
            "{agent} must not list a Decision profile: {inner}"
        );
    }
    for required in [
        "ai_aurora_profile",
        "ai_grok_profile",
        "ai_dreamer_profile",
        "ai_media_profile",
    ] {
        let inner = select_inner(&html, required);
        assert!(
            !inner.contains("value=\"\""),
            "{required} must not offer an empty profile option; select inner: {inner}"
        );
    }
    assert!(
        select_inner(&html, "ai_recap_profile").contains("value=\"\""),
        "Recap profile picker must retain an empty (off) option",
    );
}
