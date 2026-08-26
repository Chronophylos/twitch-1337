//! Integration tests for the owner-only `/settings` page.
//!
//! Drives `build_router` so the owner middleware + handler chain are
//! exercised end-to-end. The fixture seeds the session table with an
//! `owner_id` that matches the inserted Owner session so the periodic
//! role recheck inside `require_owner` admits cleanly.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::auth::Role;
use twitch_1337_web::build_router;

mod helpers;
use helpers::{
    FakeHelix, build_state_with_all_dirs, cookie_header, insert_session_as, install_crypto,
    set_owner,
};

fn empty_helix() -> Arc<FakeHelix> {
    Arc::new(FakeHelix {
        moderators: vec![],
        users: Default::default(),
    })
}

async fn body_string(res: axum::http::Response<Body>) -> String {
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

fn assert_required_profile_select_has_no_empty_option(html: &str, name: &str) {
    let inner = select_inner(html, name);
    assert!(
        !inner.contains("value=\"\""),
        "{name} must not offer an empty profile option; select inner: {inner}"
    );
}

#[tokio::test]
async fn owner_can_save_cooldown_and_handle_reflects_change() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let app = build_router(state.clone());
    // Construct a fully-populated form. The save handler treats every field
    // as authoritative, so we send the current value for everything except
    // the one knob we're changing.
    let defaults = state.settings_store.defaults().clone();
    let body = save_form_body(&bare_csrf, &defaults, &[("cooldown_ai", "15")]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
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
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let before = state.settings.load().cooldowns.ai;
    let app = build_router(state.clone());
    let defaults = state.settings_store.defaults().clone();
    // The submitted news value (900) is valid and *different* from the
    // default (60), so the re-rendered form must echo it back to verify
    // we don't discard the user's other typed input on a validation error.
    assert_ne!(
        defaults.cooldowns.news, 900,
        "fixture assumes the default news cooldown is not already 900",
    );
    // cooldown_ai = 0 violates the 1..=3600 bound.
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[("cooldown_ai", "0"), ("cooldown_news", "900")],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    assert!(
        !html.contains(&format!(
            "name=\"cooldown_news\" min=\"1\" max=\"3600\" value=\"{}\"",
            defaults.cooldowns.news
        )),
        "the stored default for news ({}) must NOT replace the submitted 900",
        defaults.cooldowns.news,
    );
    assert_eq!(
        state.settings.load().cooldowns.ai,
        before,
        "validation rejection must not mutate the live handle",
    );
}

/// Build a form body containing every required field at its current value
/// plus any extra (form_field, value) pairs the test wants to submit.
fn save_form_body(
    bare_csrf: &str,
    current: &twitch_1337_core::settings::Settings,
    extra: &[(&str, &str)],
) -> String {
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
fn read_flash(res: &axum::http::Response<Body>) -> Option<String> {
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

/// The `/settings` page no longer edits provider/base_url/model directly —
/// those live on `/ai-profiles` now. Each agent card selects its own profile.
#[tokio::test]
async fn settings_page_no_longer_offers_connection_inputs() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, _bare) = insert_session_as(&state, "123", "owner", Role::Owner);

    let app = build_router(state);
    let req = Request::builder()
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "owner GET /settings must 200");
    let html = body_string(res).await;
    assert!(
        !html.contains("ai_connection_backend"),
        "the legacy backend radio must be gone; got: {html}"
    );
    assert!(
        !html.contains("ai_connection_model"),
        "the legacy model input must be gone; got: {html}"
    );
    assert!(
        html.contains("/ai-profiles"),
        "the connection card must link to the profiles editor; got: {html}"
    );
    assert!(
        html.contains("ai_dreamer_profile"),
        "the dreamer card must offer a profile picker; got: {html}"
    );
    assert!(
        html.contains("ai_recap_profile"),
        "the Recap card must offer a profile picker; got: {html}"
    );
}

#[tokio::test]
async fn settings_nav_is_twitch_agents_shared_then_the_rest() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, _bare) = insert_session_as(&state, "123", "owner", Role::Owner);

    let req = Request::builder()
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = build_router(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "owner GET /settings must 200");
    let html = body_string(res).await;

    let twitch = html.find("data-group=\"twitch\"").expect("Twitch group");
    let agents = html.find("data-group=\"agents\"").expect("Agents group");
    let shared = html.find("data-group=\"shared\"").expect("Shared group");
    let commands = html
        .find("data-group=\"commands\"")
        .expect("Commands group");
    let emote_tracker = html
        .find("data-group=\"emote_tracker\"")
        .expect("Emote Tracker group");
    let flight_tracking = html
        .find("data-group=\"flight_tracking\"")
        .expect("Flight Tracking group");
    let system = html.find("data-group=\"system\"").expect("System group");
    assert!(twitch < agents && agents < shared && shared < commands);
    assert!(
        commands < emote_tracker && emote_tracker < flight_tracking && flight_tracking < system,
        "nav groups after Commands must be Emote Tracker → Flight Tracking → System",
    );
    assert!(!html.contains("data-group=\"ai\""));
    assert!(!html.contains("sec-ai-connection"));
    assert!(html.contains("id=\"sec-ai-aurora\""));
    assert!(html.contains("id=\"sec-ai-recap\""));
    for name in [
        "ai_aurora_profile",
        "ai_grok_profile",
        "ai_dreamer_profile",
        "ai_media_profile",
    ] {
        assert_required_profile_select_has_no_empty_option(&html, name);
    }
    let recap_inner = select_inner(&html, "ai_recap_profile");
    assert!(
        recap_inner.contains("value=\"\""),
        "Recap profile picker must retain an empty (off) option",
    );
    assert!(html.contains("name=\"ai_grok_persona_name\""));
    assert!(!html.contains("(active profile)"));
}

/// A required Dreamer profile round-trips and cannot be blank.
#[tokio::test]
async fn a_required_dreamer_profile_round_trips_through_the_settings_post() {
    use twitch_1337_core::settings::{AiProfile, AiProvider, overrides::SettingsOverrides};

    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));

    let profile = |name: &str| AiProfile {
        name: name.into(),
        key: Some("k".into()),
        provider: AiProvider::OpenRouter,
        base_url: None,
        model: "some-model".into(),
        timeout: 30,
        reasoning_effort: None,
        service_tier: None,
    };
    state
        .settings_store
        .apply(
            SettingsOverrides {
                ai: twitch_1337_core::settings::AiOverrides {
                    profiles: Some(vec![profile("default"), profile("a"), profile("b")]),
                    active_profile: Some("a".to_owned()),
                    aurora: twitch_1337_core::settings::overrides::AiAuroraOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    grok: twitch_1337_core::settings::overrides::AiGrokOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    dreamer: twitch_1337_core::settings::overrides::AiDreamerOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    media: twitch_1337_core::settings::overrides::AiMediaOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            twitch_1337_core::settings::Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            },
        )
        .await
        .expect("seed profiles");

    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);
    let current = state.settings.load().clone();

    let app = build_router(state.clone());
    let body = save_form_body(&bare_csrf, &current, &[("ai_dreamer_profile", "b")]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        state.settings.load().ai.dreamer.profile,
        "b",
        "dreamer.profile must be set to the submitted profile name",
    );

    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &state.settings.load(),
        &[("ai_dreamer_profile", "")],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    use twitch_1337_core::settings::{AiProfile, AiProvider, overrides::SettingsOverrides};

    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));

    let profile = |name: &str| AiProfile {
        name: name.into(),
        key: Some("k".into()),
        provider: AiProvider::OpenRouter,
        base_url: None,
        model: "some-model".into(),
        timeout: 30,
        reasoning_effort: None,
        service_tier: None,
    };
    state
        .settings_store
        .apply(
            SettingsOverrides {
                ai: twitch_1337_core::settings::AiOverrides {
                    profiles: Some(vec![profile("a"), profile("b")]),
                    active_profile: Some("a".to_owned()),
                    aurora: twitch_1337_core::settings::overrides::AiAuroraOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    grok: twitch_1337_core::settings::overrides::AiGrokOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    dreamer: twitch_1337_core::settings::overrides::AiDreamerOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    media: twitch_1337_core::settings::overrides::AiMediaOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            twitch_1337_core::settings::Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            },
        )
        .await
        .expect("seed profiles");

    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);
    let current = state.settings.load().clone();

    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
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
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        state.settings.load().ai.recap.profile.as_deref(),
        Some("b"),
        "recap.profile must be set to the submitted profile name",
    );
    assert_eq!(state.settings.load().ai.recap.batch_size, 30);
    assert_eq!(state.settings.load().ai.recap.max_notes, 250);
    assert_eq!(state.settings.load().ai.recap.merge_width, 25);

    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &state.settings.load(),
        &[("ai_recap_profile", "")],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        state.settings.load().ai.recap.profile,
        None,
        "an empty submission must turn Recap off, not inherit the active profile",
    );
}

#[tokio::test]
async fn grok_profile_and_persona_round_trip_through_the_settings_post() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let current = state.settings.load().clone();
    let body = save_form_body(
        &bare_csrf,
        &current,
        &[
            ("ai_grok_profile", current.ai.grok.profile.as_str()),
            ("ai_grok_persona_name", "Grok Prime"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = build_router(state.clone()).oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().ai.grok.persona_name, "Grok Prime");
}

#[tokio::test]
async fn post_settings_enabling_prefill_card_flags_restart() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    // Prefill is disabled by default; rendering the toggle card and ticking
    // the box enables it — that transition requires a restart.
    assert!(
        state.settings.load().ai.prefill.is_none(),
        "fixture assumes prefill defaults to disabled",
    );

    let app = build_router(state.clone());
    let defaults = state.settings_store.defaults().clone();
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("ai_prefill_card_visible", "1"),
            ("ai_prefill_enabled", "1"),
            ("ai_prefill_base_url", "https://logs.zonian.dev"),
            ("ai_prefill_threshold", "0.5"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    // First, enable prefill via a "card visible" save so subsequent saves
    // without the hidden input must preserve that state.
    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("ai_prefill_card_visible", "1"),
            ("ai_prefill_enabled", "1"),
            ("ai_prefill_base_url", "https://logs.zonian.dev"),
            ("ai_prefill_threshold", "0.5"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(state.settings.load().ai.prefill.is_some());

    // Now save again without rendering the card at all — prefill must stay on.
    let app = build_router(state.clone());
    let body = save_form_body(&bare_csrf, &defaults, &[]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        state.settings.load().ai.prefill.is_some(),
        "a save without the card_visible marker must NOT clear an enabled card",
    );
}

#[tokio::test]
async fn settings_page_renders_all_ai_cards() {
    // Smoke test: GETting /settings as the owner must render every AI card
    // we wired into `index.html`. Catches missing/typo'd `id="…"` anchors
    // before they break the sidebar nav and the dirty-counters JS.
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, _bare) = insert_session_as(&state, "123", "owner", Role::Owner);

    let app = build_router(state);
    let req = Request::builder()
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "owner GET /settings must 200");
    let html = body_string(res).await;
    for id in [
        "sec-ai-aurora",
        "sec-ai-grok",
        "sec-ai-recap",
        "sec-ai-dreamer",
        "sec-ai-media",
        "sec-ai-history",
        "sec-ai-prefill",
        "sec-ai-memory",
        "sec-ai-emotes",
        "sec-ai-web",
    ] {
        assert!(
            html.contains(&format!("id=\"{id}\"")),
            "settings page is missing AI card anchor {id}"
        );
    }
}

#[tokio::test]
async fn settings_page_renders_aurora_reply_length_rows() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, _bare) = insert_session_as(&state, "123", "owner", Role::Owner);

    let app = build_router(state);
    let req = Request::builder()
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    for name in [
        "ai_aurora_reply_target_chars",
        "ai_aurora_reply_soft_chars",
        "ai_aurora_reply_hard_chars",
    ] {
        assert!(
            html.contains(&format!("name=\"{name}\"")),
            "Aurora card is missing input {name}"
        );
    }
}

#[tokio::test]
async fn owner_can_save_aurora_reply_length() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("ai_aurora_reply_target_chars", "200"),
            ("ai_aurora_reply_soft_chars", "300"),
            ("ai_aurora_reply_hard_chars", "400"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let snap = state.settings.load();
    assert_eq!(snap.ai.aurora.reply_target_chars, 200);
    assert_eq!(snap.ai.aurora.reply_soft_chars, 300);
    assert_eq!(snap.ai.aurora.reply_hard_chars, 400);
}

#[tokio::test]
async fn owner_can_save_ai_history_n() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(&bare_csrf, &defaults, &[("ai_history_n", "25")]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().ai.history.n, 25);
}

#[tokio::test]
async fn ai_history_length_below_n_plus_batch_returns_400() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    assert_eq!(defaults.ai.history.n, 20);
    assert_eq!(defaults.ai.recap.batch_size, 40);

    let app = build_router(state.clone());
    let body = save_form_body(&bare_csrf, &defaults, &[("ai_history_length", "10")]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
async fn reset_ai_aurora_restores_reply_length_defaults() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("ai_aurora_reply_target_chars", "200"),
            ("ai_aurora_reply_soft_chars", "300"),
            ("ai_aurora_reply_hard_chars", "400"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let app = build_router(state.clone());
    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/settings/reset/ai_aurora")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let snap = state.settings.load();
    assert_eq!(snap.ai.aurora.reply_target_chars, 480);
    assert_eq!(snap.ai.aurora.reply_soft_chars, 490);
    assert_eq!(snap.ai.aurora.reply_hard_chars, 490);
}

#[tokio::test]
async fn reset_ai_recap_restores_recap_off_default() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let current = state.settings.load().clone();
    let body = save_form_body(
        &bare_csrf,
        &current,
        &[("ai_recap_profile", current.ai.aurora.profile.as_str())],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = build_router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(state.settings.load().ai.recap.profile.is_some());

    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/settings/reset/ai_recap")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = build_router(state.clone()).oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().ai.recap.profile, None);
}

#[tokio::test]
async fn permissions_lists_render_pretty_resting_display() {
    // hidden_admins / viewer_allowlist / emote_usage_deny_list are the
    // multi-line (textarea) rows. Like every other settings row they must
    // carry a `.row-pretty` overlay so the value is visible at rest
    // (controls are opacity:0 until hover). The textarea renders IDs
    // newline-joined ("111\n222"); only the pretty span renders them
    // comma-joined ("111, 222"), so finding that proves the span.
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    // Seed both permission lists via POST.
    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("twitch_hidden_admins", "111\n222"),
            ("twitch_viewer_allowlist", "333\n444"),
            ("twitch_emote_usage_deny_list", "evilbot\notherbot"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        state.settings.load().twitch.emote_usage_deny_list,
        vec!["evilbot".to_owned(), "otherbot".to_owned()],
        "emote_usage_deny_list must persist from POST"
    );

    // GET and confirm the pretty (comma-joined) display is present.
    let app = build_router(state);
    let req = Request::builder()
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.contains("row-pretty"),
        "permissions rows must carry a .row-pretty overlay"
    );
    assert!(
        html.contains("111, 222"),
        "hidden_admins must render a comma-joined pretty display"
    );
    assert!(
        html.contains("333, 444"),
        "viewer_allowlist must render a comma-joined pretty display"
    );
    assert!(
        html.contains("evilbot, otherbot"),
        "emote_usage_deny_list must render a comma-joined pretty display"
    );
}

#[tokio::test]
async fn settings_page_renders_ai_grok_card() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, _bare) = insert_session_as(&state, "123", "owner", Role::Owner);

    let app = build_router(state);
    let req = Request::builder()
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "owner GET /settings must 200");
    let html = body_string(res).await;
    assert!(
        html.contains("sec-ai-grok"),
        "settings page must render the Grok card section"
    );
    assert!(
        html.contains("name=\"ai_grok_profile\""),
        "Grok profile picker must be present"
    );
    assert!(
        !html.contains("name=\"ai_grok_model\""),
        "Grok model override must be gone"
    );
}

#[tokio::test]
async fn reset_cooldowns_clears_section_overrides() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    // First push a non-default value via /settings POST.
    let app = build_router(state.clone());
    let body = save_form_body(&bare_csrf, &defaults, &[("cooldown_ai", "15")]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().cooldowns.ai, 15);

    // Then reset just the cooldowns section.
    let app = build_router(state.clone());
    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/settings/reset/cooldowns")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "reset should redirect on success",
    );
    assert_eq!(
        state.settings.load().cooldowns.ai,
        defaults.cooldowns.ai,
        "reset must restore the compiled default",
    );
}

#[tokio::test]
async fn dreamer_checkbox_absent_sets_enabled_false() {
    // The dreamer card always renders (no *_card_visible pattern), so
    // omitting ai_dreamer_enabled from the POST means "disabled".
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let app = build_router(state.clone());
    let defaults = state.settings_store.defaults().clone();
    // Send the form without ai_dreamer_enabled.
    let body = save_form_body(&bare_csrf, &defaults, &[]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    // First, enable emotes so there's an existing include_global value to preserve.
    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("ai_emotes_card_visible", "1"),
            ("ai_emotes_enabled", "1"),
            ("ai_emotes_include_global", "1"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    let app = build_router(state.clone());
    let body = save_form_body(&bare_csrf, &defaults, &[]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("ai_emotes_card_visible", "1"),
            ("ai_emotes_enabled", "1"),
            ("ai_emotes_max_prompt_emotes", "20"),
            ("ai_emotes_pinned_emotes", "PepeLa\nokjj\nClueless"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
async fn media_size_malformed_string_is_ignored() {
    // A malformed byte-size string (e.g. "not-a-size") must not crash the
    // handler; the field parses to None so the previous value is preserved.
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    // Submit a valid save first so we have a known max_image_size.
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[("ai_media_max_image_size", "5 MB")],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    // Now submit a malformed size: handler must succeed (not 500).
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[("ai_media_max_image_size", "not-a-size")],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert!(
        res.status().is_redirection() || res.status().is_client_error(),
        "malformed size must not cause a 500; got {}",
        res.status(),
    );
}

// ---------------------------------------------------------------------------
// Tasks 21–25: new settings cards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn twitch_permissions_save_persists() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    // owner is now bootstrap-only (config.toml), not a form field.
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("twitch_hidden_admins", "111\n222"),
            ("twitch_emote_usage_deny_list", "evilbot\notherbot"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "twitch_permissions save must redirect"
    );
    let s = state.settings.load();
    assert_eq!(
        s.twitch.hidden_admins,
        vec!["111".to_owned(), "222".to_owned()],
        "hidden_admins must persist",
    );
    assert_eq!(
        s.twitch.emote_usage_deny_list,
        vec!["evilbot".to_owned(), "otherbot".to_owned()],
        "emote_usage_deny_list must persist",
    );
}

#[tokio::test]
async fn twitch_channels_save_persists() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(&bare_csrf, &defaults, &[("twitch_expected_latency", "200")]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    // 1500 exceeds the 0..=1000 bound.
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[("twitch_expected_latency", "1500")],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "out-of-range expected_latency must return 400",
    );
}

#[tokio::test]
async fn aviationstack_save_persists() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("aviationstack_card_visible", "1"),
            ("aviationstack_enabled", "1"),
            ("aviationstack_base_url", "https://api.example.com/v1"),
            ("aviationstack_timeout_secs", "10"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[("suspend_default_duration_secs", "1800")],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("web_session_ttl_secs", "14400"),
            ("web_mod_check_refresh_secs", "60"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
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

#[tokio::test]
async fn reset_twitch_permissions_clears_section() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    // Push non-default values. owner is bootstrap-only (config.toml), not a form field.
    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(&bare_csrf, &defaults, &[("twitch_hidden_admins", "111")]);
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();
    assert_eq!(
        state.settings.load().twitch.hidden_admins,
        vec!["111".to_owned()]
    );

    // Reset the section.
    let app = build_router(state.clone());
    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/settings/reset/twitch_permissions")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "reset must redirect");
    assert!(
        state.settings.load().twitch.hidden_admins.is_empty(),
        "reset must clear hidden_admins",
    );
}

#[tokio::test]
async fn reset_aviationstack_clears_section() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);

    // Push a non-default enabled state.
    let defaults = state.settings_store.defaults().clone();
    let app = build_router(state.clone());
    let body = save_form_body(
        &bare_csrf,
        &defaults,
        &[
            ("aviationstack_card_visible", "1"),
            ("aviationstack_enabled", "1"),
            ("aviationstack_timeout_secs", "30"),
        ],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();
    assert!(state.settings.load().aviationstack.enabled);

    // Reset the section.
    let app = build_router(state.clone());
    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/settings/reset/aviationstack")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "reset must redirect");
    assert_eq!(
        state.settings.load().aviationstack,
        defaults.aviationstack,
        "aviationstack must be restored to defaults after reset",
    );
}

#[tokio::test]
async fn settings_page_renders_new_cards() {
    // Smoke-test: GET /settings must render all 5 new card section anchors.
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));
    let (sid, csrf_cookie, _bare) = insert_session_as(&state, "123", "owner", Role::Owner);

    let app = build_router(state);
    let req = Request::builder()
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "owner GET /settings must 200");
    let html = body_string(res).await;
    for id in [
        "sec-twitch_permissions",
        "sec-twitch_channels",
        "sec-aviationstack",
        "sec-suspend",
        "sec-web_runtime",
    ] {
        assert!(
            html.contains(&format!("id=\"{id}\"")),
            "settings page is missing new card anchor {id}",
        );
    }
}

/// A validation error must re-render untouched cards from the *stored*
/// settings, not from compiled defaults: the whole page posts as one form,
/// so a card echoed at its default gets submitted as an explicit
/// "set to default" on the corrective save and wipes settings silently.
#[tokio::test]
async fn validation_error_preserves_stored_ai_settings() {
    use twitch_1337_core::settings::{AiProfile, AiProvider, overrides::SettingsOverrides};

    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    set_owner(&state, Some("123"));

    let profile = |name: &str| AiProfile {
        name: name.into(),
        key: Some("k".into()),
        provider: AiProvider::OpenRouter,
        base_url: None,
        model: "some-model".into(),
        timeout: 30,
        reasoning_effort: None,
        service_tier: None,
    };
    state
        .settings_store
        .apply(
            SettingsOverrides {
                ai: twitch_1337_core::settings::AiOverrides {
                    profiles: Some(vec![profile("a"), profile("b")]),
                    active_profile: Some("a".to_owned()),
                    aurora: twitch_1337_core::settings::overrides::AiAuroraOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    grok: twitch_1337_core::settings::overrides::AiGrokOverrides {
                        profile: Some(Some("b".into())),
                        ..Default::default()
                    },
                    dreamer: twitch_1337_core::settings::overrides::AiDreamerOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    media: twitch_1337_core::settings::overrides::AiMediaOverrides {
                        profile: Some(Some("a".into())),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            twitch_1337_core::settings::Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            },
        )
        .await
        .expect("seed grok profile");

    let (sid, csrf_cookie, bare_csrf) = insert_session_as(&state, "123", "owner", Role::Owner);
    let current = state.settings.load().clone();

    // Fail validation on a different card: the inject budget must cover
    // SOUL + LORE, and 1024 bytes does not.
    let body = save_form_body(
        &bare_csrf,
        &current,
        &[("ai_memory_inject_byte_budget", "1024")],
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = build_router(state.clone()).oneshot(req).await.unwrap();
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
