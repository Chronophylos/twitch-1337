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
    Actor, AiAuroraOverrides, AiDreamerOverrides, AiEmotesAgentOverrides, AiGrokOverrides,
    AiMediaOverrides, AiOverrides, AiProfile, AiProvider, ProfileKind, Settings, SettingsOverrides,
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

const PILL_EDITOR_PATHS: [&str; 12] = [
    "twitch.hidden_admins",
    "twitch.viewer_allowlist",
    "twitch.emote_usage_deny_list",
    "ai.emotes.pinned_emotes",
    "reply_emotes.invalid",
    "reply_emotes.denied",
    "reply_emotes.cooldown",
    "reply_emotes.timeout",
    "reply_emotes.limit",
    "reply_emotes.failure",
    "reply_emotes.unavailable",
    "volanta.pilots",
];

/// Fresh state owned by [`OWNER_ID`]; keep the tempdirs alive for the test.
async fn owner_state() -> (WebState, (TempDir, TempDir, TempDir)) {
    install_crypto();
    let helix = Arc::new(FakeHelix {
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
        ("cooldowns.ai".to_owned(), current.cooldowns.ai.to_string()),
        (
            "cooldowns.news".to_owned(),
            current.cooldowns.news.to_string(),
        ),
        ("cooldowns.up".to_owned(), current.cooldowns.up.to_string()),
        (
            "cooldowns.feedback".to_owned(),
            current.cooldowns.feedback.to_string(),
        ),
        (
            "cooldowns.doener".to_owned(),
            current.cooldowns.doener.to_string(),
        ),
        (
            "pings.cooldown".to_owned(),
            current.pings.cooldown.to_string(),
        ),
        (
            "ai.aurora.profile".to_owned(),
            current.ai.aurora.profile.clone(),
        ),
        (
            "ai.grok.profile".to_owned(),
            current.ai.grok.profile.clone(),
        ),
        (
            "ai.dreamer.profile".to_owned(),
            current.ai.dreamer.profile.clone(),
        ),
        (
            "ai.media.profile".to_owned(),
            current.ai.media.profile.clone(),
        ),
        (
            "bash.max_concurrent".to_owned(),
            current.bash.max_concurrent.to_string(),
        ),
        (
            "bash.timeout_ms".to_owned(),
            current.bash.timeout_ms.to_string(),
        ),
        (
            "bash.max_stdout_bytes".to_owned(),
            current.bash.max_stdout_bytes.to_string(),
        ),
        (
            "bash.max_vfs_bytes".to_owned(),
            current.bash.max_vfs_bytes.to_string(),
        ),
        (
            "cooldowns.bash".to_owned(),
            current.cooldowns.bash.to_string(),
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
/// Dreamer, Emotes and Media; Grok points at `grok_profile`.
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
                    emotes_agent: AiEmotesAgentOverrides {
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
        .save(&state, &defaults, &[("cooldowns.ai", "15")])
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
    // cooldowns.ai = 0 violates the 1..=3600 bound.
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[("cooldowns.ai", "0"), ("cooldowns.news", "900")],
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
    Box::pin(seed_chat_profiles(&state, &["default", "a", "b"], "a")).await;

    let owner = Owner::login(&state);
    let current = state.settings.load();
    let res = owner
        .save(&state, &current, &[("ai.dreamer.profile", "b")])
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        state.settings.load().ai.dreamer.profile,
        "b",
        "dreamer.profile must be set to the submitted profile name",
    );

    let current = state.settings.load();
    let res = owner
        .save(&state, &current, &[("ai.dreamer.profile", "")])
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
    Box::pin(seed_chat_profiles(&state, &["a", "b"], "a")).await;

    let owner = Owner::login(&state);
    let current = state.settings.load();
    let res = owner
        .save(
            &state,
            &current,
            &[
                ("ai.recap.profile", "b"),
                ("ai.recap.batch_size", "30"),
                ("ai.recap.max_notes", "250"),
                ("ai.recap.merge_width", "25"),
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
        .save(&state, &current, &[("ai.recap.profile", "")])
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
                ("ai.grok.profile", current.ai.grok.profile.as_str()),
                ("ai.grok.persona_name", "Grok Prime"),
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
                ("ai.prefill.__shown", "1"),
                ("ai.prefill.enabled", "1"),
                ("ai.prefill.base_url", "https://logs.zonian.dev"),
                ("ai.prefill.threshold", "0.5"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let flash = read_flash(&res).expect("save must emit a flash cookie");
    assert!(
        flash.contains("ai.prefill (enabling requires restart)"),
        "enabling prefill must mention the section in the restart hint; got {flash:?}",
    );
    assert!(
        state.settings.load().ai.prefill.is_some(),
        "prefill must be enabled after apply",
    );
}

#[tokio::test]
async fn post_settings_disabling_emotes_card_flags_restart() {
    let (state, _dirs) = owner_state().await;
    let owner = Owner::login(&state);
    let defaults = state.settings_store.defaults().clone();
    let res = owner
        .save(
            &state,
            &defaults,
            &[("ai.emotes.__shown", "1"), ("ai.emotes.enabled", "1")],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(state.settings.load().ai.emotes.is_some());

    let current = state.settings.load();
    let res = owner
        .save(&state, &current, &[("ai.emotes.__shown", "1")])
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(state.settings.load().ai.emotes.is_none());
    let flash = read_flash(&res).expect("save must emit a flash cookie");
    assert!(
        flash.contains("Restart bot to apply: ai.emotes (disabling requires restart)"),
        "disabling emotes must render the restart marker; got {flash:?}",
    );

    let res = owner.get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    let card_start = html.find("<section id=\"sec-ai-emotes\"").unwrap();
    let card_end = card_start + html[card_start..].find("</section>").unwrap();
    let card = &html[card_start..card_end];
    assert!(card.contains("is-card-off"));
    assert!(card.contains("class=\"restart-required\">restart required</span>"));
    assert!(card.contains("Enabling or disabling requires a restart."));
}

#[tokio::test]
async fn post_settings_card_invisible_leaves_toggle_alone() {
    // Regression for the `<path>.__shown` sentinel pattern: a save that
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
                ("ai.prefill.__shown", "1"),
                ("ai.prefill.enabled", "1"),
                ("ai.prefill.base_url", "https://logs.zonian.dev"),
                ("ai.prefill.threshold", "0.5"),
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
        "a save without the ai.prefill.__shown sentinel must NOT clear an enabled card",
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
                ("ai.aurora.reply_target_chars", "200"),
                ("ai.aurora.reply_soft_chars", "300"),
                ("ai.aurora.reply_hard_chars", "400"),
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
        .save(&state, &defaults, &[("ai.history.n", "25")])
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
        .save(&state, &defaults, &[("ai.history.length", "10")])
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
async fn permissions_lists_render_saved_values_in_canonical_inputs() {
    let (state, _dirs) = owner_state().await;
    let owner = Owner::login(&state);

    // The parser remains deliberately semantic-free: nonnumeric IDs and
    // differently cased entries persist in their posted order.
    let defaults = state.settings_store.defaults().clone();
    let res = owner
        .save(
            &state,
            &defaults,
            &[
                ("twitch.hidden_admins", "not-a-number\nAdmin\nadmin"),
                ("twitch.viewer_allowlist", "333\n444"),
                ("twitch.emote_usage_deny_list", "evilbot\notherbot"),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let s = state.settings.load();
    assert_eq!(
        s.twitch.hidden_admins,
        vec![
            "not-a-number".to_owned(),
            "Admin".to_owned(),
            "admin".to_owned()
        ],
        "hidden_admins must preserve nonnumeric, case-distinct entries in order",
    );
    assert_eq!(
        s.twitch.viewer_allowlist,
        vec!["333".to_owned(), "444".to_owned()],
        "viewer_allowlist must persist from POST",
    );
    assert_eq!(
        s.twitch.emote_usage_deny_list,
        vec!["evilbot".to_owned(), "otherbot".to_owned()],
        "emote_usage_deny_list must persist from POST"
    );

    // The old textareas are gone; each hidden input carries exactly the same
    // newline-delimited value the settings parser already accepts.
    let res = owner.get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    for (name, joined) in [
        ("twitch.hidden_admins", "not-a-number\nAdmin\nadmin"),
        ("twitch.viewer_allowlist", "333\n444"),
        ("twitch.emote_usage_deny_list", "evilbot\notherbot"),
    ] {
        let tag = tag_with_name(&html, name);
        assert!(tag.starts_with("<input"), "{name} must be an input");
        assert_eq!(attr(tag, "type"), Some("hidden"), "{name} canonical input");
        assert_eq!(attr(tag, "value"), Some(joined), "{name} ordered value");
    }
}

#[tokio::test]
async fn dreamer_checkbox_absent_with_sentinel_sets_enabled_false() {
    // With the `ai.dreamer.__shown` sentinel posted, omitting
    // `ai.dreamer.enabled` means the box was unticked: "disabled".
    let (state, _dirs) = owner_state().await;

    let res = post_settings(
        &state,
        &[("ai.dreamer.__shown", "1"), ("ai.dreamer.enabled", "1")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(state.settings.load().ai.dreamer.enabled);

    // Without the sentinel, the absent checkbox is "no change".
    let res = post_settings(&state, &[]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        state.settings.load().ai.dreamer.enabled,
        "absent checkbox without the sentinel must leave dreamer.enabled alone",
    );

    let res = post_settings(&state, &[("ai.dreamer.__shown", "1")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        !state.settings.load().ai.dreamer.enabled,
        "missing ai.dreamer.enabled with the sentinel must set dreamer.enabled = false",
    );
}

#[tokio::test]
async fn emotes_card_include_global_untouched_without_sentinel() {
    // When ai.emotes.__shown is not submitted (card not rendered),
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
                ("ai.emotes.__shown", "1"),
                ("ai.emotes.enabled", "1"),
                ("ai.emotes.include_global", "1"),
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

    // Now submit a form without the sentinel: include_global must stay.
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
        "include_global must not change when ai.emotes.__shown was absent",
    );
}

#[tokio::test]
async fn emotes_card_pinned_emotes_round_trips_in_displayed_order() {
    let (state, _dirs) = owner_state().await;
    let owner = Owner::login(&state);
    let defaults = state.settings_store.defaults().clone();
    let res = owner
        .save(
            &state,
            &defaults,
            &[
                ("ai.emotes.__shown", "1"),
                ("ai.emotes.enabled", "1"),
                ("ai.emotes.max_prompt_emotes", "20"),
                ("ai.emotes.pinned_emotes", "Clueless\nPepeLa\nokjj"),
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
        Some(vec!["Clueless".into(), "PepeLa".into(), "okjj".into()]),
        "pinned_emotes must preserve the editor's order",
    );

    let res = owner.get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    let canonical = tag_with_name(&html, "ai.emotes.pinned_emotes");
    assert_eq!(attr(canonical, "type"), Some("hidden"));
    assert_eq!(attr(canonical, "value"), Some("Clueless\nPepeLa\nokjj"));
    let editor_at = html
        .find("data-pill-editor=\"ai.emotes.pinned_emotes\"")
        .expect("pinned emote pill editor");
    let editor_tag = &html[editor_at..html[editor_at..].find('>').unwrap() + editor_at + 1];
    assert!(editor_tag.contains("data-pill-reorder"));
}

#[tokio::test]
async fn reply_emotes_card_persists_pools_and_empty_means_disabled() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(
            &state,
            &defaults,
            &[
                ("reply_emotes.invalid", "OnlyThis\nSecondEmote"),
                ("reply_emotes.failure", ""),
            ],
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let current = state.settings.load();
    assert_eq!(current.reply_emotes.invalid, ["OnlyThis", "SecondEmote"]);
    assert!(current.reply_emotes.failure.is_empty());
}

#[tokio::test]
async fn reply_emote_pool_validation_returns_400_with_pool_field() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let owner = Owner::login(&state);
    let too_many = (0..21)
        .map(|n| format!("Emote{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let cases = [
        "two words".to_owned(),
        "bad\u{1}token".to_owned(),
        "x".repeat(26),
        "Same\nSame".to_owned(),
        too_many,
    ];

    for value in cases {
        let res = owner
            .save(
                &state,
                &defaults,
                &[("reply_emotes.invalid", value.as_str())],
            )
            .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "value {value:?}");
        let body = body_string(res).await;
        assert!(
            body.contains("reply_emotes.invalid"),
            "validation error must identify the pool path: {body}"
        );
    }
}

#[tokio::test]
async fn twitch_channels_save_persists() {
    let (state, _dirs) = owner_state().await;
    let defaults = state.settings_store.defaults().clone();
    let res = Owner::login(&state)
        .save(&state, &defaults, &[("twitch.expected_latency", "200")])
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
        .save(&state, &defaults, &[("twitch.expected_latency", "1500")])
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
                ("aviationstack.__shown", "1"),
                ("aviationstack.enabled", "1"),
                ("aviationstack.base_url", "https://api.example.com/v1"),
                ("aviationstack.timeout_secs", "10"),
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
            &[("suspend.default_duration_secs", "1800")],
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
                ("web.session_ttl_secs", "14400"),
                ("web.mod_check_refresh_secs", "60"),
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
    Box::pin(seed_chat_profiles(&state, &["a", "b"], "b")).await;

    let current = state.settings.load();
    // Fail validation on a different card: the inject budget must cover
    // SOUL + LORE, and 1024 bytes does not.
    let res = Owner::login(&state)
        .save(
            &state,
            &current,
            &[("ai.memory.inject_byte_budget", "1024")],
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
        html.contains("name=\"ai.grok.profile\"") && html.contains("value=\"b\" selected"),
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
    Box::pin(seed_decision_profile(&state, false)).await;
    let res = post_settings(
        &state,
        &[
            ("spoiler.profile", "dec"),
            ("spoiler.default_game", "Elden Ring"),
            ("spoiler.confidence_floor", "65"),
            ("cooldowns.spoiler", "45"),
            ("cooldowns.servers", "50"),
            ("cooldowns.aktien", "90"),
            ("cooldowns.help", "20"),
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
    assert_eq!(s.cooldowns.aktien, 90);
    assert_eq!(s.cooldowns.help, 20);
}

#[tokio::test]
async fn empty_spoiler_profile_persists_as_off() {
    let (state, _dirs) = owner_state().await;
    Box::pin(seed_decision_profile(&state, true)).await;
    assert_eq!(state.settings.load().spoiler.profile, "dec");
    let res = post_settings(
        &state,
        &[
            ("spoiler.profile", ""),
            ("spoiler.default_game", "Hades 2"),
            ("spoiler.confidence_floor", "70"),
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
    Box::pin(seed_decision_profile(&state, true)).await;
    let res = post_settings(&state, &[("cooldowns.ai", "99")]).await;
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
    Box::pin(seed_decision_profile(&state, true)).await;
    let before = state.settings.load().spoiler.clone();
    let res = post_settings(
        &state,
        &[
            ("spoiler.profile", "dec"),
            ("spoiler.default_game", "Hades 2"),
            ("spoiler.confidence_floor", "70"),
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
    Box::pin(seed_decision_profile(&state, false)).await;
    let res = Owner::login(&state).get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("id=\"sec-spoiler\""), "spoiler card missing");
    assert!(html.contains("name=\"cooldowns.spoiler\""));
    assert!(html.contains("name=\"cooldowns.servers\""));
    assert!(html.contains("name=\"cooldowns.aktien\""));
    assert!(html.contains("name=\"cooldowns.help\""));
    let inner = select_inner(&html, "spoiler.profile");
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
    assert!(
        html.contains("id=\"sec-feedback\""),
        "feedback card missing"
    );
    let feedback = select_inner(&html, "feedback.profile");
    assert!(
        feedback.contains("value=\"\""),
        "feedback classifier is optional: {feedback}"
    );
    assert!(feedback.contains("value=\"dec\""), "{feedback}");
    for n in &chat_names {
        assert!(
            !feedback.contains(&format!("value=\"{n}\"")),
            "chat profile {n} leaked into feedback dropdown"
        );
    }
    for agent in [
        "ai.aurora.profile",
        "ai.grok.profile",
        "ai.dreamer.profile",
        "ai.media.profile",
        "ai.recap.profile",
    ] {
        let inner = select_inner(&html, agent);
        assert!(
            !inner.contains("value=\"dec\""),
            "{agent} must not list a Decision profile: {inner}"
        );
    }
    for required in [
        "ai.aurora.profile",
        "ai.grok.profile",
        "ai.dreamer.profile",
        "ai.media.profile",
    ] {
        let inner = select_inner(&html, required);
        assert!(
            !inner.contains("value=\"\""),
            "{required} must not offer an empty profile option; select inner: {inner}"
        );
    }
    assert!(
        select_inner(&html, "ai.recap.profile").contains("value=\"\""),
        "Recap profile picker must retain an empty (off) option",
    );
}

/// Return the opening tag (`<input …>`, `<select …>`, …) that carries
/// `name="{name}"`.
fn tag_with_name<'a>(html: &'a str, name: &str) -> &'a str {
    let marker = format!("name=\"{name}\"");
    let at = html
        .find(&marker)
        .unwrap_or_else(|| panic!("missing element with {marker}"));
    let start = html[..at].rfind('<').expect("tag open");
    let end = at + html[at..].find('>').expect("tag close");
    &html[start..=end]
}

/// The settings row containing the control named `name`.
fn settings_row_with_name<'a>(form: &'a str, name: &str) -> &'a str {
    let marker = format!("name=\"{name}\"");
    let at = form
        .find(&marker)
        .unwrap_or_else(|| panic!("missing element with {marker}"));
    let start = form[..at]
        .rfind("<div class=\"settings-row\"")
        .expect("settings row open");
    let next_row = form[at..]
        .find("<div class=\"settings-row\"")
        .map(|rel| at + rel);
    let section_end = form[at..].find("</section>").map(|rel| at + rel);
    let end = next_row
        .into_iter()
        .chain(section_end)
        .min()
        .unwrap_or(form.len());
    &form[start..end]
}

/// The markup between `<form id="settings-form"` and its `</form>`.
fn settings_form(html: &str) -> &str {
    let start = html
        .find("<form id=\"settings-form\"")
        .expect("settings form present");
    let end = start + html[start..].find("</form>").expect("settings form closed");
    &html[start..end]
}

/// Value of `attr="…"` inside a single tag (attributes may be separated by
/// any whitespace).
fn attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let marker = format!("{attr}=\"");
    let mut from = 0;
    while let Some(rel) = tag[from..].find(&marker) {
        let at = from + rel;
        if tag[..at].ends_with(char::is_whitespace) {
            let v = &tag[at + marker.len()..];
            return Some(&v[..v.find('"')?]);
        }
        from = at + marker.len();
    }
    None
}

/// POST only `_csrf` plus `fields` (no baseline form fields).
async fn post_raw(state: &WebState, fields: &[(&str, &str)]) -> Response<Body> {
    let owner = Owner::login(state);
    let body = std::iter::once(("_csrf", owner.csrf.as_str()))
        .chain(fields.iter().copied())
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    owner.post(state, "/settings", body).await
}

/// Every form knob in the table has a rendered control, every rendered
/// control is a form knob (or `_csrf` / a `.__shown` sentinel), and number
/// inputs carry exactly the table's min/max.
#[tokio::test]
async fn settings_form_names_match_the_knob_table() {
    let (state, _dirs) = owner_state().await;
    let res = Owner::login(&state).get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    let form = settings_form(&html);

    let mut names = std::collections::BTreeSet::new();
    let mut from = 0;
    while let Some(rel) = form[from..].find("name=\"") {
        let at = from + rel;
        let v = &form[at + "name=\"".len()..];
        let end = v.find('"').expect("closed name attr");
        if form[..at].ends_with(char::is_whitespace) {
            names.insert(v[..end].to_owned());
        }
        from = at + "name=\"".len() + end;
    }

    let knobs = twitch_1337_core::settings::knobs();
    let form_paths: std::collections::BTreeSet<&str> = knobs
        .iter()
        .filter(|k| k.form)
        .map(|k| k.path.as_str())
        .collect();
    for path in &form_paths {
        assert!(names.contains(*path), "form knob {path} is not rendered");
    }
    for name in &names {
        assert!(
            form_paths.contains(name.as_str()) || name == "_csrf" || name.ends_with(".__shown"),
            "rendered name {name} is not a form knob path",
        );
    }

    for k in knobs.iter().filter(|k| k.form) {
        let tag = tag_with_name(form, &k.path);
        if attr(tag, "type") != Some("number") {
            continue;
        }
        assert_eq!(attr(tag, "min"), k.min.as_deref(), "{} min", k.path);
        assert_eq!(attr(tag, "max"), k.max.as_deref(), "{} max", k.path);
    }
    let ai = tag_with_name(form, "cooldowns.ai");
    assert_eq!(attr(ai, "min"), Some("1"));
    assert_eq!(attr(ai, "max"), Some("3600"));
    assert_eq!(
        attr(tag_with_name(form, "ai.web.max_rounds"), "max"),
        Some("6")
    );
}

#[tokio::test]
async fn settings_rows_render_restart_badges_from_knob_table() {
    let (state, _dirs) = owner_state().await;
    let res = Owner::login(&state).get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    let form = settings_form(&html);

    for path in [
        "ai.web.base_url",
        "ai.web.timeout",
        "ai.web.cache_ttl_secs",
        "ai.web.cache_capacity",
    ] {
        let k = twitch_1337_core::settings::knobs()
            .iter()
            .find(|k| k.path == path)
            .expect("web connection knob");
        assert!(k.badge(), "{path} must be restart-required");
        let row = settings_row_with_name(form, path);
        assert!(
            row.contains("<span class=\"restart-required\">restart required</span>"),
            "{path} must render its restart marker",
        );
    }
}

#[tokio::test]
async fn unparseable_value_returns_400_and_changes_nothing() {
    let (state, _dirs) = owner_state().await;
    let before = state.settings.load().cooldowns.ai;
    let res = post_raw(&state, &[("cooldowns.ai", "abc")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let html = body_string(res).await;
    assert!(
        html.contains("cooldowns.ai") && html.contains("invalid value"),
        "response must surface the invalid field; got: {html}",
    );
    assert_eq!(state.settings.load().cooldowns.ai, before);
}

/// #621: a malformed byte size used to parse as "no override" and silently
/// reset the knob to its compiled default.
#[tokio::test]
async fn malformed_media_size_returns_400_and_keeps_saved_value() {
    let (state, (_p, _m, settings_dir)) = owner_state().await;
    let res = post_settings(&state, &[("ai.media.max_image_size", "5 MB")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let settings_ron = settings_dir.path().join("settings.ron");
    let saved = std::fs::read(&settings_ron).expect("settings.ron persisted");

    for bad in ["not-a-size", "5 MBB"] {
        let res = post_settings(&state, &[("ai.media.max_image_size", bad)]).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
        let html = body_string(res).await;
        assert!(
            html.contains("<code>ai.media.max_image_size</code>: invalid value"),
            "{bad}: response must surface the invalid field; got: {html}",
        );
        assert_eq!(
            state.settings.load().ai.media.max_image_size.as_u64(),
            5_000_000,
            "{bad}"
        );
        assert_eq!(std::fs::read(&settings_ron).unwrap(), saved, "{bad}");
    }
}

const BYTE_SIZE_PATHS: [&str; 10] = [
    "ai.media.max_image_size",
    "ai.media.max_pdf_size",
    "ai.media.max_audio_size",
    "ai.media.max_video_size",
    "ai.media.max_text_size",
    "ai.memory.soul_bytes",
    "ai.memory.lore_bytes",
    "ai.memory.user_bytes",
    "ai.memory.scratch_bytes",
    "ai.memory.inject_byte_budget",
];

/// #640: a 0 byte-size limit rejects every fetch or memory write, so each of
/// the ten byte-size knobs must refuse 0 with a field error and keep the
/// stored settings untouched.
#[tokio::test]
async fn zero_byte_size_returns_400_with_field_error() {
    let (state, (_p, _m, settings_dir)) = owner_state().await;
    let settings_ron = settings_dir.path().join("settings.ron");
    let res = post_settings(&state, &[]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let saved = std::fs::read(&settings_ron).expect("settings.ron persisted");
    let before = state.settings.load().ai.clone();

    for path in BYTE_SIZE_PATHS {
        let res = post_settings(&state, &[(path, "0")]).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path}");
        let html = body_string(res).await;
        assert!(
            html.contains(&format!("<code>{path}</code>: must be")),
            "{path}: response must surface the min-bound field error; got: {html}",
        );
        assert_eq!(state.settings.load().ai, before, "{path}");
        assert_eq!(std::fs::read(&settings_ron).unwrap(), saved, "{path}");
    }
}

/// #640: 1 byte is the smallest accepted value for every byte-size knob. The
/// inject budget must still cover SOUL + LORE, so it is posted as 2.
#[tokio::test]
async fn one_byte_size_is_accepted() {
    let (state, _dirs) = owner_state().await;
    let fields: Vec<(&str, &str)> = BYTE_SIZE_PATHS
        .iter()
        .map(|&p| {
            (
                p,
                if p == "ai.memory.inject_byte_budget" {
                    "2"
                } else {
                    "1"
                },
            )
        })
        .collect();
    let res = post_settings(&state, &fields).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let ai = state.settings.load().ai.clone();
    for size in [
        ai.media.max_image_size,
        ai.media.max_pdf_size,
        ai.media.max_audio_size,
        ai.media.max_video_size,
        ai.media.max_text_size,
    ] {
        assert_eq!(size.as_u64(), 1);
    }
    assert_eq!(ai.memory.soul_bytes, 1);
    assert_eq!(ai.memory.lore_bytes, 1);
    assert_eq!(ai.memory.user_bytes, 1);
    assert_eq!(ai.memory.scratch_bytes, 1);
    assert_eq!(ai.memory.inject_byte_budget, 2);
}

/// The four Bash rows and the `!bash` cooldown save and read back, proving the
/// card is wired to the same knob table the derive generates.
#[tokio::test]
async fn bash_card_save_persists() {
    let (state, _dirs) = owner_state().await;
    let res = post_settings(
        &state,
        &[
            ("bash.timeout_ms", "2500"),
            ("bash.max_stdout_bytes", "512"),
            ("bash.max_vfs_bytes", "8192"),
            ("cooldowns.bash", "45"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let saved = state.settings.load();
    assert_eq!(saved.bash.timeout_ms, 2500);
    assert_eq!(saved.bash.max_stdout_bytes, 512);
    assert_eq!(saved.bash.max_vfs_bytes, 8192);
    assert_eq!(saved.cooldowns.bash, 45);
    assert_eq!(
        saved.bash.max_concurrent,
        Settings::compiled_defaults().bash.max_concurrent,
        "an untouched row keeps its default",
    );
}

/// `bash.max_concurrent` sizes the interpreter slot pool at startup, so the
/// save must tell the owner a restart is required and name the knob.
#[tokio::test]
async fn bash_max_concurrent_flashes_restart() {
    let (state, _dirs) = owner_state().await;
    let res = post_settings(&state, &[("bash.max_concurrent", "3")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().bash.max_concurrent, 3);
    let flash = read_flash(&res).expect("save must emit a flash cookie");
    assert!(
        flash.contains("Restart bot to apply: bash.max_concurrent"),
        "restart flash must list bash.max_concurrent; got {flash:?}",
    );
}

/// The six Commands · Execution rows save, read back, and apply live: no
/// restart flash, the handle carries the new limits right after the save.
#[tokio::test]
async fn commands_execution_card_save_persists_without_restart() {
    let (state, _dirs) = owner_state().await;
    let res = post_settings(
        &state,
        &[
            ("commands.execution.max_active", "16"),
            ("commands.execution.max_pending", "0"),
            ("commands.execution.max_active_per_user", "2"),
            ("commands.execution.max_pending_per_user", "0"),
            ("commands.execution.max_active_per_command", "4"),
            ("commands.execution.max_pending_per_command", "0"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let flash = read_flash(&res);
    assert!(
        flash.as_deref().is_none_or(|f| !f.contains("Restart")),
        "execution limits apply live; got {flash:?}",
    );
    let snapshot = state.settings.load();
    let saved = &snapshot.commands.execution;
    assert_eq!(saved.max_active, 16);
    assert_eq!(saved.max_pending, 0);
    assert_eq!(saved.max_active_per_user, 2);
    assert_eq!(saved.max_pending_per_user, 0);
    assert_eq!(saved.max_active_per_command, 4);
    assert_eq!(saved.max_pending_per_command, 0);
}

/// Every execution knob outside its constant range is a 400 naming that
/// knob's dotted path, and nothing is saved.
#[tokio::test]
async fn commands_execution_out_of_range_returns_400_on_knob_path() {
    let (state, _dirs) = owner_state().await;
    let before = state.settings.load().commands.execution.clone();
    for (knob, bad) in [
        ("max_active", "0"),
        ("max_active", "129"),
        ("max_pending", "1025"),
        ("max_pending", "-1"),
        ("max_active_per_user", "0"),
        ("max_active_per_user", "33"),
        ("max_pending_per_user", "65"),
        ("max_active_per_command", "0"),
        ("max_active_per_command", "65"),
        ("max_pending_per_command", "65"),
    ] {
        let field = format!("commands.execution.{knob}");
        let res = post_raw(&state, &[(field.as_str(), bad)]).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{field}={bad}");
        let html = body_string(res).await;
        assert!(
            html.contains(&format!("<code>{field}</code>")),
            "{field}={bad}: error must name the field; got: {html}",
        );
        assert_eq!(
            state.settings.load().commands.execution,
            before,
            "{field}={bad}"
        );
    }
}

/// A per-user or per-command limit above its global counterpart is a 400 on
/// the scoped knob's dotted path, and nothing is saved.
#[tokio::test]
async fn commands_execution_cross_field_violation_returns_400_on_scoped_knob() {
    let (state, _dirs) = owner_state().await;
    let before = state.settings.load().commands.execution.clone();
    let cases: [(&str, &[(&str, &str)]); 4] = [
        (
            "max_active_per_user",
            &[
                ("commands.execution.max_active", "4"),
                ("commands.execution.max_active_per_command", "4"),
                ("commands.execution.max_active_per_user", "5"),
            ],
        ),
        (
            "max_active_per_command",
            &[
                ("commands.execution.max_active", "4"),
                ("commands.execution.max_active_per_command", "5"),
            ],
        ),
        (
            "max_pending_per_user",
            &[
                ("commands.execution.max_pending", "4"),
                ("commands.execution.max_pending_per_command", "4"),
                ("commands.execution.max_pending_per_user", "5"),
            ],
        ),
        (
            "max_pending_per_command",
            &[
                ("commands.execution.max_pending", "4"),
                ("commands.execution.max_pending_per_user", "4"),
                ("commands.execution.max_pending_per_command", "5"),
            ],
        ),
    ];
    for (knob, fields) in cases {
        let res = post_raw(&state, fields).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{knob}");
        let html = body_string(res).await;
        assert!(
            html.contains(&format!("<code>commands.execution.{knob}</code>")),
            "{knob}: error must name the scoped knob; got: {html}",
        );
        assert_eq!(state.settings.load().commands.execution, before, "{knob}");
    }
}

#[tokio::test]
async fn changing_a_restart_knob_flashes_its_path() {
    let (state, _dirs) = owner_state().await;
    assert_ne!(state.settings.load().aviationstack.timeout_secs, 20);
    let res = post_raw(
        &state,
        &[
            ("aviationstack.__shown", "1"),
            ("aviationstack.enabled", "1"),
            ("aviationstack.timeout_secs", "20"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let flash = read_flash(&res).expect("save must emit a flash cookie");
    assert!(
        flash.contains("Restart bot to apply: ") && flash.contains("aviationstack.timeout_secs"),
        "restart flash must list the changed knob; got {flash:?}",
    );
}

#[tokio::test]
async fn old_flat_field_name_is_rejected_as_unknown() {
    let (state, _dirs) = owner_state().await;
    let before = state.settings.load().cooldowns.ai;
    let res = post_raw(&state, &[("cooldown_ai", "45")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let html = body_string(res).await;
    assert!(
        html.contains("unknown field"),
        "old flat names must be rejected; got: {html}"
    );
    assert_eq!(state.settings.load().cooldowns.ai, before);
}

#[tokio::test]
async fn link_correction_card_renders_all_controls() {
    let (state, _dirs) = owner_state().await;
    let res = Owner::login(&state).get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("sec-link_correction"), "card section present");
    for name in [
        "link_correction.__shown",
        "link_correction.mydealz",
        "link_correction.reddit",
        "link_correction.x_twitter",
        "link_correction.max_links",
    ] {
        assert!(
            html.contains(&format!("name=\"{name}\"")),
            "{name} rendered"
        );
    }
    let cap = tag_with_name(&html, "link_correction.max_links");
    assert_eq!(attr(cap, "min"), Some("1"));
    assert_eq!(attr(cap, "max"), Some("5"));
    // The hint is plain text escaped exactly once: `<sub>` must reach the
    // browser as an entity, never as a double-escaped `&lt;` literal (#703).
    assert!(
        html.contains("/r/&#60;sub&#62;/s/&#60;id&#62;")
            || html.contains("/r/&lt;sub&gt;/s/&lt;id&gt;"),
        "reddit hint escaped once"
    );
    assert!(
        !html.contains("&#38;lt;") && !html.contains("&amp;lt;"),
        "no double-escaped entities in settings hints"
    );
}

#[tokio::test]
async fn link_correction_checkboxes_round_trip_both_directions() {
    let (state, _dirs) = owner_state().await;

    // Sentinel posted; only reddit ticked; cap 4. The other two checkboxes
    // are omitted, so they must save as false.
    let res = post_raw(
        &state,
        &[
            ("link_correction.__shown", "1"),
            ("link_correction.reddit", "1"),
            ("link_correction.max_links", "4"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let lc = &state.settings.load().link_correction;
    assert!(!lc.mydealz);
    assert!(lc.reddit);
    assert!(!lc.x_twitter);
    assert_eq!(lc.max_links, 4);

    // Second form flips membership: mydealz + x_twitter ticked, reddit
    // omitted, cap back to 2.
    let res = post_raw(
        &state,
        &[
            ("link_correction.__shown", "1"),
            ("link_correction.mydealz", "1"),
            ("link_correction.x_twitter", "1"),
            ("link_correction.max_links", "2"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let lc = &state.settings.load().link_correction;
    assert!(lc.mydealz);
    assert!(!lc.reddit);
    assert!(lc.x_twitter);
    assert_eq!(lc.max_links, 2);

    // Without the sentinel, an absent checkbox is "no change".
    let res = post_raw(&state, &[("link_correction.max_links", "3")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let lc = &state.settings.load().link_correction;
    assert!(lc.mydealz && lc.x_twitter && !lc.reddit);
    assert_eq!(lc.max_links, 3);
}

#[tokio::test]
async fn link_correction_cap_bounds_rejected_without_change() {
    let (state, _dirs) = owner_state().await;
    for bad in ["0", "6"] {
        let res = post_raw(
            &state,
            &[
                ("link_correction.__shown", "1"),
                ("link_correction.reddit", "1"),
                ("link_correction.max_links", bad),
            ],
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "cap {bad}");
        let html = body_string(res).await;
        assert!(
            html.contains("link_correction.max_links"),
            "error must name the field; got: {html}"
        );
    }
    assert_eq!(state.settings.load().link_correction.max_links, 2);
}

#[tokio::test]
async fn f1_alerts_card_renders_and_round_trips() {
    let (state, _dirs) = owner_state().await;
    let res = Owner::login(&state).get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("sec-alerts_f1"), "card section present");
    let poll = tag_with_name(&html, "alerts.f1.poll_secs");
    assert_eq!(attr(poll, "min"), Some("60"));
    assert_eq!(attr(poll, "max"), Some("86400"));

    let f1 = state.settings.load().alerts.f1.clone();
    assert!(f1.enabled, "on by default");
    assert_eq!(f1.poll_secs, 3600);

    // Sentinel posted, checkbox omitted: saves as off.
    let res = post_raw(
        &state,
        &[("alerts.f1.__shown", "1"), ("alerts.f1.poll_secs", "1800")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let f1 = state.settings.load().alerts.f1.clone();
    assert!(!f1.enabled);
    assert_eq!(f1.poll_secs, 1800);

    let res = post_raw(
        &state,
        &[
            ("alerts.f1.__shown", "1"),
            ("alerts.f1.enabled", "1"),
            ("alerts.f1.poll_secs", "3600"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let f1 = state.settings.load().alerts.f1.clone();
    assert!(f1.enabled);
    assert_eq!(f1.poll_secs, 3600);
}

/// Every row is labelled with the knob's short name, never its dotted path.
/// Every newline-list knob uses the reusable pill editor in a wide row and
/// keeps its canonical value in a hidden input, not a textarea.
#[tokio::test]
async fn settings_rows_use_short_labels_and_wide_pill_editors() {
    let (state, _dirs) = owner_state().await;
    let res = Owner::login(&state).get(&state, "/settings").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    let form = settings_form(&html);

    let mut labels = 0;
    for chunk in form.split("<code class=\"row-key\">").skip(1) {
        let label = &chunk[..chunk.find("</code>").expect("closed row key")];
        assert!(!label.contains('.'), "row label {label:?} is a dotted path");
        labels += 1;
    }
    assert!(labels > 50, "settings form lost its rows ({labels})");

    for name in PILL_EDITOR_PATHS {
        let canonical = tag_with_name(form, name);
        assert!(
            canonical.starts_with("<input"),
            "{name} must not be a textarea"
        );
        assert_eq!(attr(canonical, "type"), Some("hidden"), "{name}");
        assert_eq!(attr(canonical, "class"), Some("pill-canonical"), "{name}");
        assert!(
            form.contains(&format!("id=\"{name}_entry\"")),
            "{name} entry input missing"
        );

        let marker = format!("data-pill-editor=\"{name}\"");
        let at = form
            .find(&marker)
            .unwrap_or_else(|| panic!("{name} pill editor"));
        let row_start = form[..at].rfind("<div class=\"settings-row").expect("row");
        let row_tag = &form[row_start..row_start + form[row_start..].find('>').expect("row tag")];
        assert!(row_tag.contains("row-wide"), "{name} is not in a wide row");
    }
    assert_eq!(
        form.matches("data-pill-editor=\"").count(),
        PILL_EDITOR_PATHS.len(),
        "unexpected pill editor count"
    );
}
