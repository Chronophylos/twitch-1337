//! Integration tests for /ai-profiles CRUD (owner-gated — issue I1: a
//! profile carries `base_url`/`key`, the same credential surface that was
//! owner-only on `main`).

use std::{collections::HashMap, fs, sync::Arc};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use secrecy::SecretString;
use tower::ServiceExt as _;
use twitch_1337_core::settings::{Actor, AiProfile, AiProvider, SettingsOverrides};
use twitch_1337_web::build_router;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header as request_header, method, path},
};

use super::helpers;
use helpers::{
    FakeHelix, build_state_with_all_dirs, cookie_header, insert_session_as, install_crypto,
};

fn empty_helix() -> Arc<FakeHelix> {
    Arc::new(FakeHelix {
        users: Default::default(),
    })
}

async fn body_string(res: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn profile(name: &str) -> AiProfile {
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

/// Apply profiles via the store so we can seed pre-test state without going
/// through the dashboard.
async fn seed_profiles(state: &twitch_1337_web::WebState, rows: Vec<AiProfile>, active: &str) {
    state
        .settings_store
        .apply(
            SettingsOverrides {
                ai: twitch_1337_core::settings::AiOverrides {
                    profiles: Some(rows),
                    aurora: twitch_1337_core::settings::AiAuroraOverrides {
                        profile: Some(Some(active.to_owned())),
                        ..Default::default()
                    },
                    grok: twitch_1337_core::settings::AiGrokOverrides {
                        profile: Some(Some(active.to_owned())),
                        ..Default::default()
                    },
                    dreamer: twitch_1337_core::settings::AiDreamerOverrides {
                        profile: Some(Some(active.to_owned())),
                        ..Default::default()
                    },
                    media: twitch_1337_core::settings::AiMediaOverrides {
                        profile: Some(Some(active.to_owned())),
                        ..Default::default()
                    },
                    recap: twitch_1337_core::settings::AiRecapOverrides {
                        profile: Some(None),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            },
        )
        .await
        .expect("seed profiles");
}

#[tokio::test]
async fn delete_refuses_to_remove_the_last_profile() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("only")], "only")).await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/only/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK); // re-rendered with an error
    assert_eq!(state.settings.load().ai.profiles.len(), 1);
}

/// I1: `/ai-profiles` moved from mod-gated to owner-gated because a profile
/// carries `base_url` and `key`, the same credential surface `/settings`
/// kept owner-only on `main`. A mod (who was admitted here before this fix)
/// must now be refused; an owner must still be admitted.
#[tokio::test]
async fn the_page_is_owner_gated() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;

    let (sid, csrf_cookie, _bare_csrf) =
        insert_session_as(&state, "111", "viewer", twitch_1337_web::auth::Role::Viewer);
    let app = build_router(state.clone());
    let req = Request::builder()
        .uri("/ai-profiles")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "a viewer must be refused"
    );

    let (sid, csrf_cookie, _bare_csrf) =
        insert_session_as(&state, "999", "mod", twitch_1337_web::auth::Role::Mod);
    let app = build_router(state.clone());
    let req = Request::builder()
        .uri("/ai-profiles")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "a mod must be refused");

    let (sid, csrf_cookie, _bare_csrf) =
        insert_session_as(&state, "123", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());
    let req = Request::builder()
        .uri("/ai-profiles")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "an owner must be admitted");
}

/// I1, mutating side: a mod's POST must be refused before it ever reaches
/// the handler, not just the page GET.
#[tokio::test]
async fn mod_cannot_delete_a_profile() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a"), profile("b")], "a")).await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "mod", twitch_1337_web::auth::Role::Mod);
    let app = build_router(state.clone());

    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/b/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        state.settings.load().ai.profiles.len(),
        2,
        "a mod's delete must not take effect"
    );
}

#[tokio::test]
async fn add_rejects_an_effort_the_provider_does_not_support() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let body = format!(
        "_csrf={csrf}&name=new&provider=ollama&model=qwen3%3A8b&timeout=30&reasoning_effort=high",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/add")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(state.settings.load().ai.profiles.len(), 1);
}

#[tokio::test]
async fn add_profile_persists_and_renders() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let body = format!(
        "_csrf={csrf}&name=fresh&provider=open_ai&model=gpt-5&timeout=45&key=k",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/add")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let listed = {
        let req = Request::builder()
            .uri("/ai-profiles")
            .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        body_string(res).await
    };
    assert!(listed.contains("fresh"), "row must appear in list");
    let v = state.settings.load().ai.profiles.clone();
    assert_eq!(v.len(), 2);
    assert!(v.iter().any(|p| p.name == "fresh"));
}

#[tokio::test]
async fn add_rejects_openai_profile_without_key() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let body = format!(
        "_csrf={csrf}&name=fresh&provider=open_ai&model=gpt-5&timeout=45",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/add")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "must re-render the form");
    let html = body_string(res).await;
    assert_invalid_control(&html, "name=\"key\"", "profile-new-key-error");
    assert!(
        error_element(&html, "profile-new-key-error").contains("required"),
        "validation must explain the key field inline; got: {html}"
    );
    assert!(
        !html.contains("form.key"),
        "raw form paths must not leak into the page: {html}"
    );
    assert_eq!(
        state.settings.load().ai.profiles.len(),
        1,
        "the keyless row must not persist"
    );
}

#[tokio::test]
async fn edit_renaming_profile_rewrites_all_five_agent_pointers() {
    install_crypto();
    let (state, _td_p, _td_m, td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("old")], "old")).await;
    state
        .settings_store
        .apply(
            SettingsOverrides {
                ai: twitch_1337_core::settings::AiOverrides {
                    recap: twitch_1337_core::settings::AiRecapOverrides {
                        profile: Some(Some("old".into())),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            },
        )
        .await
        .expect("seed recap pointer");
    let settings_path = td_s.path().join("settings.ron");
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let body = format!(
        "_csrf={csrf}&name=renamed&provider=open_router&model=some-model&timeout=30&key=k",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/old/edit")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let html = body_string(res).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
    let settings = state.settings.load();
    assert_eq!(settings.ai.profiles.len(), 1);
    assert_eq!(settings.ai.profiles[0].name, "renamed");
    assert_eq!(settings.ai.aurora.profile, "renamed");
    assert_eq!(settings.ai.grok.profile, "renamed");
    assert_eq!(settings.ai.dreamer.profile, "renamed");
    assert_eq!(settings.ai.media.profile, "renamed");
    assert_eq!(settings.ai.recap.profile.as_deref(), Some("renamed"));

    let persisted: SettingsOverrides =
        ron::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
    assert_eq!(persisted.ai.aurora.profile, Some(Some("renamed".into())));
    assert_eq!(persisted.ai.grok.profile, Some(Some("renamed".into())));
    assert_eq!(persisted.ai.dreamer.profile, Some(Some("renamed".into())));
    assert_eq!(persisted.ai.media.profile, Some(Some("renamed".into())));
    assert_eq!(persisted.ai.recap.profile, Some(Some("renamed".into())));
}

#[tokio::test]
async fn delete_refuses_every_referenced_agent_pointer() {
    for pointer in ["aurora", "grok", "dreamer", "media", "recap"] {
        install_crypto();
        let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
        Box::pin(seed_profiles(&state, vec![profile("a"), profile("b")], "a")).await;
        let mut ai = twitch_1337_core::settings::AiOverrides::default();
        match pointer {
            "aurora" => ai.aurora.profile = Some(Some("b".into())),
            "grok" => ai.grok.profile = Some(Some("b".into())),
            "dreamer" => ai.dreamer.profile = Some(Some("b".into())),
            "media" => ai.media.profile = Some(Some("b".into())),
            "recap" => ai.recap.profile = Some(Some("b".into())),
            _ => unreachable!(),
        }
        state
            .settings_store
            .apply(
                SettingsOverrides {
                    ai,
                    ..Default::default()
                },
                Actor {
                    user_id: "test".into(),
                    user_login: "test".into(),
                },
            )
            .await
            .expect("seed agent pointer");
        let (sid, csrf_cookie, bare_csrf) =
            insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
        let app = build_router(state.clone());

        let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
        let req = Request::builder()
            .method("POST")
            .uri("/ai-profiles/b/delete")
            .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "{pointer} reference must block delete"
        );
        assert_eq!(
            state.settings.load().ai.profiles.len(),
            2,
            "{pointer} reference must preserve the profile"
        );
    }
}

#[tokio::test]
async fn delete_allows_unreferenced_profile_when_recap_is_off() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a"), profile("b")], "a")).await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/b/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let settings = state.settings.load();
    assert_eq!(settings.ai.profiles.len(), 1);
    assert_eq!(settings.ai.profiles[0].name, "a");
    assert_eq!(settings.ai.recap.profile, None);
}

/// A rename that also trips store-level validation (an unsupported
/// `reasoning_effort` here) must re-render the edit form pointed at the
/// ORIGINAL name, not the rejected rename — otherwise the user's fix-up
/// resubmit POSTs to a profile that was never created and the store's
/// `find(|p| p.name == target)` silently matches nothing, "succeeding"
/// without ever renaming anything.
#[tokio::test]
async fn edit_rename_with_rejected_field_reopens_under_the_original_name_then_succeeds() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("old")], "old")).await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    // Step 1: rename "old" -> "renamed" while also picking an effort the
    // provider (ollama, which allows none) does not support. The store
    // rejects this; the re-rendered form must still target "old".
    let bad_body = format!(
        "_csrf={csrf}&name=renamed&provider=ollama&model=qwen3%3A8b&timeout=30&reasoning_effort=high",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/old/edit")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(bad_body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "validation error re-renders");
    let html = body_string(res).await;
    assert!(
        html.contains("action=\"/ai-profiles/old/edit\""),
        "re-rendered edit form must still target the original name, not the \
         rejected rename; body was: {html}"
    );
    assert!(
        !html.contains("action=\"/ai-profiles/renamed/edit\""),
        "must not target the nonexistent renamed profile"
    );
    let settings = state.settings.load();
    assert_eq!(settings.ai.profiles.len(), 1);
    assert_eq!(
        settings.ai.profiles[0].name, "old",
        "rejected rename must not persist"
    );
    assert_eq!(settings.ai.aurora.profile, "old");

    // Step 2: resubmit the corrected edit to the action the page actually
    // rendered (still "/ai-profiles/old/edit") — the rename must land, and
    // Since Aurora named "old", its pointer must follow it.
    let good_body = format!(
        "_csrf={csrf}&name=renamed&provider=ollama&model=qwen3%3A8b&timeout=30",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/old/edit")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(good_body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let settings = state.settings.load();
    assert_eq!(settings.ai.profiles.len(), 1);
    assert_eq!(
        settings.ai.profiles[0].name, "renamed",
        "rename must actually land"
    );
    assert_eq!(settings.ai.aurora.profile, "renamed");
}

/// Profile names are only checked for being non-blank. The rendered edit
/// action must reach the same profile through the router when the name holds
/// `/`, `%` or `#`, instead of 404ing or matching a different profile.
#[tokio::test]
async fn edit_form_action_round_trips_names_with_reserved_chars() {
    install_crypto();
    let name = "a/b 50%#x";
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(
        &state,
        vec![profile(name), profile("a")],
        "a",
    ))
    .await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let req = Request::builder()
        .uri(format!("/ai-profiles?edit={}", urlencoding::encode(name)))
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let html = body_string(app.clone().oneshot(req).await.unwrap()).await;
    let action = html
        .split("action=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .find(|a| a.ends_with("/edit"))
        .unwrap_or_else(|| panic!("no edit form action rendered; body was: {html}"))
        .to_owned();

    let body = format!(
        "_csrf={csrf}&name=renamed&provider=open_router&model=m2&timeout=30&key=k",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let res = post_form(app, &sid, &csrf_cookie, &action, body).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "POST {action}");
    let settings = state.settings.load();
    let mut names: Vec<_> = settings
        .ai
        .profiles
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["a", "renamed"],
        "POST {action} must rename {name:?}"
    );
}

fn decision(name: &str) -> AiProfile {
    AiProfile {
        kind: twitch_1337_core::settings::ProfileKind::Decision,
        ..profile(name)
    }
}

async fn post_form(
    app: axum::Router,
    sid: &str,
    csrf_cookie: &str,
    uri: &str,
    body: String,
) -> axum::http::Response<Body> {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie_header(sid, csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

fn model_listing_form(
    csrf: &str,
    provider: &str,
    base_url: &str,
    key: &str,
    timeout: u64,
) -> String {
    format!(
        "_csrf={}&name=draft&provider={}&base_url={}&key={}&model=manual-model&timeout={timeout}",
        urlencoding::encode(csrf),
        urlencoding::encode(provider),
        urlencoding::encode(base_url),
        urlencoding::encode(key),
    )
}

async fn post_models(
    app: axum::Router,
    sid: &str,
    csrf_cookie: &str,
    body: String,
) -> axum::http::Response<Body> {
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/models")
        .header(header::COOKIE, cookie_header(sid, csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("HX-Request", "true")
        .body(Body::from(body))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

fn use_listing_key(state: &mut twitch_1337_web::WebState) {
    state.llm = Arc::new(
        twitch_1337_core::llm_factory::LlmRegistry::new(None, state.settings.clone()).with_keys(
            HashMap::from([("k".to_owned(), SecretString::from("test-secret"))]),
        ),
    );
}

async fn assert_safe_listing_error(res: axum::http::Response<Body>, forbidden: &[&str]) {
    assert!(
        res.status().is_client_error() || res.status().is_server_error(),
        "listing failures must not be swapped as successful fragments"
    );
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(content_type.starts_with("text/plain"), "{content_type}");
    let text = body_string(res).await;
    assert!(!text.is_empty());
    assert!(text.len() <= 300, "toast text must stay short: {text}");
    assert!(!text.contains("<input"), "{text}");
    assert!(!text.contains("manual-model"), "{text}");
    for value in forbidden {
        assert!(!text.contains(value), "sensitive value leaked: {text}");
    }
}

async fn point_spoiler_at(state: &twitch_1337_web::WebState, name: &str) {
    let mut o = SettingsOverrides::default();
    o.spoiler.profile = Some(name.to_owned());
    state
        .settings_store
        .apply(
            o,
            Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            },
        )
        .await
        .expect("point spoiler");
}

#[tokio::test]
async fn add_decision_profile_on_open_router_persists_kind() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=dec&kind=decision&provider=open_router&model=m&timeout=30&key=k",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/add",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let s = state.settings.load();
    let p = s.ai.profiles.iter().find(|p| p.name == "dec").unwrap();
    assert_eq!(p.kind, twitch_1337_core::settings::ProfileKind::Decision);
}

#[tokio::test]
async fn add_decision_profile_on_other_provider_is_rejected() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=dec&kind=decision&provider=open_ai&model=m&timeout=30&key=k",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/add",
        body,
    )
    .await;
    assert!(res.status().is_client_error() || res.status() == StatusCode::OK);
    assert_ne!(res.status(), StatusCode::SEE_OTHER);
    let html = body_string(res).await;
    assert!(html.contains("Validation failed"), "{html}");
    assert!(
        html.contains("open_router") || html.contains("OpenRouter"),
        "{html}"
    );
    assert_eq!(state.settings.load().ai.profiles.len(), 1);
}

#[tokio::test]
async fn editing_a_decision_profile_keeps_its_kind() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(
        &state,
        vec![profile("a"), decision("dec")],
        "a",
    ))
    .await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=dec&kind=decision&provider=open_router&model=new-model&timeout=30&key=k",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/dec/edit",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let s = state.settings.load();
    let p = s.ai.profiles.iter().find(|p| p.name == "dec").unwrap();
    assert_eq!(p.kind, twitch_1337_core::settings::ProfileKind::Decision);
    assert_eq!(p.model, "new-model");
}

#[tokio::test]
async fn edit_form_preselects_the_profile_kind() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(
        &state,
        vec![profile("a"), decision("dec")],
        "a",
    ))
    .await;
    let (sid, cc, _) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let req = Request::builder()
        .uri("/ai-profiles?edit=dec")
        .header(header::COOKIE, cookie_header(&sid, &cc))
        .body(Body::empty())
        .unwrap();
    let res = build_router(state.clone()).oneshot(req).await.unwrap();
    let html = body_string(res).await;
    assert!(html.contains("name=\"kind\""), "{html}");
    assert!(
        html.contains("value=\"decision\" selected"),
        "decision must be preselected"
    );
}

#[tokio::test]
async fn renaming_the_spoiler_profile_rewrites_spoiler_pointer() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(
        &state,
        vec![profile("a"), decision("dec")],
        "a",
    ))
    .await;
    point_spoiler_at(&state, "dec").await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=dec2&kind=decision&provider=open_router&model=m&timeout=30&key=k",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/dec/edit",
        body,
    )
    .await;
    let status = res.status();
    assert_eq!(status, StatusCode::SEE_OTHER, "{}", body_string(res).await);
    assert_eq!(state.settings.load().spoiler.profile, "dec2");
}

#[tokio::test]
async fn delete_refuses_the_spoiler_profile() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(
        &state,
        vec![profile("a"), decision("dec")],
        "a",
    ))
    .await;
    point_spoiler_at(&state, "dec").await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!("_csrf={}", urlencoding::encode(&csrf));
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/dec/delete",
        body,
    )
    .await;
    assert_ne!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().ai.profiles.len(), 2);
}

#[tokio::test]
async fn test_connection_uses_unsaved_values_without_persisting_the_profile() {
    install_crypto();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "OK" }
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=draft&provider=ollama&base_url={}&model=unsaved-model&timeout=30",
        urlencoding::encode(&csrf),
        urlencoding::encode(&upstream.uri()),
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/test",
        body,
    )
    .await;

    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("Connection successful"), "{html}");
    assert!(html.contains("value=\"unsaved-model\""), "{html}");
    assert_eq!(state.settings.load().ai.profiles.len(), 1);
    assert_eq!(state.settings.load().ai.profiles[0].name, "a");
}

#[tokio::test]
async fn test_connection_sends_openrouter_credentials_and_advanced_options() {
    install_crypto();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(request_header("authorization", "Bearer test-secret"))
        .and(body_partial_json(serde_json::json!({
            "model": "draft-openrouter",
            "messages": [{ "role": "user", "content": "Reply with OK." }],
            "reasoning": { "effort": "high" },
            "service_tier": "priority"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{ "message": { "content": "OK" } }]
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    let (mut state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    state.llm = Arc::new(
        twitch_1337_core::llm_factory::LlmRegistry::new(None, state.settings.clone()).with_keys(
            HashMap::from([("k".to_owned(), SecretString::from("test-secret"))]),
        ),
    );
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=draft&provider=open_router&base_url={}&model=draft-openrouter&timeout=30&key=k&reasoning_effort=high&service_tier=priority",
        urlencoding::encode(&csrf),
        urlencoding::encode(&upstream.uri()),
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/test",
        body,
    )
    .await;

    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("Connection successful"), "{html}");
    assert!(
        !html.contains("test-secret"),
        "API secret must not be rendered"
    );
    assert_eq!(state.settings.load().ai.profiles.len(), 1);
}

fn profile_card<'a>(html: &'a str, name: &str) -> &'a str {
    let title = format!("card-title\">{name}</span>");
    let card = html
        .split("<article")
        .find(|part| part.contains(&title))
        .unwrap_or_else(|| panic!("{name} profile card missing: {html}"));
    card.split_once("</article>").map_or(card, |(card, _)| card)
}

#[tokio::test]
async fn cards_label_metadata_and_warn_about_missing_models() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    let mut chat = profile("chat");
    chat.base_url = Some("https://example.com/v1".into());
    chat.reasoning_effort = Some("high".into());
    chat.service_tier = Some("flex".into());
    let mut empty = profile("empty");
    empty.model.clear();
    empty.key = None;
    let mut whitespace = profile("whitespace");
    whitespace.model = " \t ".into();
    Box::pin(seed_profiles(
        &state,
        vec![decision("decision"), chat, empty, whitespace],
        "chat",
    ))
    .await;
    let (sid, cc, _csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let req = Request::builder()
        .uri("/ai-profiles")
        .header(header::COOKIE, cookie_header(&sid, &cc))
        .body(Body::empty())
        .unwrap();
    let html = body_string(build_router(state).oneshot(req).await.unwrap()).await;

    let chat = profile_card(&html, "chat");
    assert!(chat.contains(">Chat</span>"), "missing Chat kind: {chat}");
    for label in [
        "Kind",
        "Provider",
        "Model",
        "Base URL",
        "Key name",
        "Timeout",
        "Reasoning effort",
        "Service tier",
    ] {
        assert!(chat.contains(label), "missing {label} label: {chat}");
    }
    for value in [
        "OpenRouter",
        "some-model",
        "https://example.com/v1",
        "30 seconds",
        "high",
        "flex",
    ] {
        assert!(chat.contains(value), "missing {value} value: {chat}");
    }

    let decision = profile_card(&html, "decision");
    assert!(
        decision.contains(">Decision</span>"),
        "missing Decision kind: {decision}"
    );
    for absent in ["Base URL", "Reasoning effort", "Service tier"] {
        assert!(
            !decision.contains(absent),
            "absent {absent} option must not render: {decision}"
        );
    }

    for name in ["empty", "whitespace"] {
        let missing = profile_card(&html, name);
        assert!(
            missing.contains("No model configured"),
            "{name} model needs a visible warning: {missing}"
        );
        assert!(
            !missing.contains("<code"),
            "{name} model must not render a value element: {missing}"
        );
    }
    let empty = profile_card(&html, "empty");
    assert!(
        !empty.contains("Key name"),
        "absent key name must not render: {empty}"
    );
}

#[tokio::test]
async fn load_models_returns_openrouter_datalist_without_caching() {
    install_crypto();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(request_header("authorization", "Bearer test-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {
                    "id": "vendor/vision",
                    "name": "Vision Model",
                    "context_length": 131072,
                    "architecture": {
                        "input_modalities": ["text", "image"],
                        "output_modalities": ["text"]
                    }
                },
                {
                    "id": "vendor/image-only",
                    "name": "Image Only",
                    "context_length": 8192,
                    "architecture": {
                        "input_modalities": ["text"],
                        "output_modalities": ["image"]
                    }
                }
            ]
        })))
        .expect(2)
        .mount(&upstream)
        .await;
    let (mut state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    use_listing_key(&mut state);
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());
    let body = model_listing_form(&csrf, "open_router", &upstream.uri(), "k", 30);

    let res = post_models(app.clone(), &sid, &cc, body.clone()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.contains("<datalist id=\"profile-new-model-options\">"),
        "{html}"
    );
    assert!(html.contains("value=\"vendor/vision\""), "{html}");
    assert!(html.contains("Vision Model"), "{html}");
    assert!(html.contains("131072 tokens"), "{html}");
    assert!(html.contains("image input"), "{html}");
    assert!(!html.contains("vendor/image-only"), "{html}");
    assert!(!html.contains("test-secret"), "{html}");
    assert!(!html.contains("name=\"model\""), "{html}");

    let second = post_models(app, &sid, &cc, body).await;
    assert_eq!(second.status(), StatusCode::OK);
}

#[tokio::test]
async fn model_datalist_does_not_restrict_free_text_save() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());
    let req = Request::builder()
        .uri("/ai-profiles?new=true")
        .header(header::COOKIE, cookie_header(&sid, &cc))
        .body(Body::empty())
        .unwrap();
    let html = body_string(app.clone().oneshot(req).await.unwrap()).await;
    assert!(html.contains("name=\"model\""), "{html}");
    assert!(
        html.contains("list=\"profile-new-model-options\""),
        "{html}"
    );
    assert!(
        html.contains("<datalist id=\"profile-new-model-options\">"),
        "{html}"
    );
    assert!(html.contains("Load models"), "{html}");

    let body = format!(
        "_csrf={}&name=manual&provider=open_ai&model=not-in-any-provider-list&timeout=30&key=k",
        urlencoding::encode(&csrf)
    );
    let res = post_form(app, &sid, &cc, "/ai-profiles/add", body).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let settings = state.settings.load();
    let saved = settings
        .ai
        .profiles
        .iter()
        .find(|p| p.name == "manual")
        .unwrap();
    assert_eq!(saved.model, "not-in-any-provider-list");
}

#[tokio::test]
async fn model_listing_errors_are_short_plain_text_and_do_not_replace_the_input() {
    install_crypto();
    let (mut state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    use_listing_key(&mut state);
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let not_found = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(404).set_body_string("UPSTREAM-404-BODY test-secret"))
        .expect(1)
        .mount(&not_found)
        .await;
    let res = post_models(
        app.clone(),
        &sid,
        &cc,
        model_listing_form(&csrf, "open_ai_compatible", &not_found.uri(), "k", 30),
    )
    .await;
    assert_safe_listing_error(res, &["test-secret", "UPSTREAM-404-BODY"]).await;

    let unauthorized = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("UPSTREAM-401-BODY test-secret"))
        .expect(1)
        .mount(&unauthorized)
        .await;
    let res = post_models(
        app.clone(),
        &sid,
        &cc,
        model_listing_form(&csrf, "open_ai", &unauthorized.uri(), "k", 30),
    )
    .await;
    assert_safe_listing_error(res, &["test-secret", "UPSTREAM-401-BODY"]).await;

    let malformed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("UPSTREAM-MALFORMED-BODY test-secret"),
        )
        .expect(1)
        .mount(&malformed)
        .await;
    let res = post_models(
        app.clone(),
        &sid,
        &cc,
        model_listing_form(&csrf, "open_ai", &malformed.uri(), "k", 30),
    )
    .await;
    assert_safe_listing_error(res, &["test-secret", "UPSTREAM-MALFORMED-BODY"]).await;

    let res = post_models(
        app.clone(),
        &sid,
        &cc,
        model_listing_form(&csrf, "open_ai", "", "missing", 30),
    )
    .await;
    assert_safe_listing_error(res, &["test-secret"]).await;

    let res = post_models(
        app.clone(),
        &sid,
        &cc,
        model_listing_form(&csrf, "open_ai", "", "", 30),
    )
    .await;
    assert_safe_listing_error(res, &["test-secret"]).await;

    let slow = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(2)))
        .expect(1)
        .mount(&slow)
        .await;
    let res = post_models(
        app,
        &sid,
        &cc,
        model_listing_form(&csrf, "open_ai", &slow.uri(), "k", 1),
    )
    .await;
    assert_safe_listing_error(res, &["test-secret"]).await;
}

#[tokio::test]
async fn model_listing_route_is_owner_only_and_csrf_checked() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    for (user_id, role) in [
        ("111", twitch_1337_web::auth::Role::Viewer),
        ("999", twitch_1337_web::auth::Role::Mod),
    ] {
        let (sid, cc, csrf) = insert_session_as(&state, user_id, "not-owner", role);
        let res = post_models(
            build_router(state.clone()),
            &sid,
            &cc,
            model_listing_form(&csrf, "open_router", "http://127.0.0.1:9", "k", 1),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    let (sid, cc, _) =
        insert_session_as(&state, "123", "owner", twitch_1337_web::auth::Role::Owner);
    let res = post_models(
        build_router(state),
        &sid,
        &cc,
        model_listing_form("wrong-csrf", "open_router", "http://127.0.0.1:9", "k", 1),
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cards_show_assignment_links_and_name_delete_blockers() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(
        &state,
        vec![profile("active"), profile("unused"), decision("classifier")],
        "active",
    ))
    .await;
    state
        .settings_store
        .apply(
            SettingsOverrides {
                ai: twitch_1337_core::settings::AiOverrides {
                    recap: twitch_1337_core::settings::AiRecapOverrides {
                        profile: Some(Some("active".into())),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            },
        )
        .await
        .expect("assign recap");
    point_spoiler_at(&state, "classifier").await;
    let (sid, cc, _csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let req = Request::builder()
        .uri("/ai-profiles")
        .header(header::COOKIE, cookie_header(&sid, &cc))
        .body(Body::empty())
        .unwrap();
    let html = body_string(build_router(state).oneshot(req).await.unwrap()).await;

    for (label, anchor) in [
        ("Aurora", "sec-ai-aurora"),
        ("Grok", "sec-ai-grok"),
        ("Dreamer", "sec-ai-dreamer"),
        ("Media", "sec-ai-media"),
        ("Recap", "sec-ai-recap"),
        ("Spoiler check", "sec-spoiler"),
    ] {
        assert!(
            html.contains(&format!("href=\"/settings#{anchor}\"")) && html.contains(label),
            "missing {label} assignment link: {html}"
        );
    }
    let unused = profile_card(&html, "unused");
    assert!(unused.contains("Not assigned"), "{unused}");
    assert!(
        unused.contains("data-confirm=\"Delete profile “unused”?\""),
        "unassigned delete confirmation changed: {unused}"
    );
    assert!(
        unused.contains("name=\"_csrf\""),
        "delete form must retain its CSRF token: {unused}"
    );
    assert!(
        html.contains("Reassign Aurora, Grok, Dreamer, Media, Recap before deleting"),
        "delete confirmation must name blockers: {html}"
    );
}

// ─── Inline field errors (#677) ──────────────────────────────────────────────

/// Opening tags (`<input …>`, `<select …>`, `<div …>`) that contain `needle`.
fn tags_containing<'a>(html: &'a str, needle: &str) -> Vec<&'a str> {
    html.match_indices(needle)
        .filter_map(|(at, _)| {
            let start = html[..at].rfind('<')?;
            let end = at + html[at..].find('>')?;
            Some(&html[start..=end])
        })
        .collect()
}

/// Exactly one control matching `needle` is marked invalid, and it points
/// `aria-describedby` at `error_id`.
fn assert_invalid_control(html: &str, needle: &str, error_id: &str) {
    let invalid: Vec<&str> = tags_containing(html, needle)
        .into_iter()
        .filter(|tag| tag.contains("aria-invalid=\"true\""))
        .collect();
    assert_eq!(
        invalid.len(),
        1,
        "expected one invalid {needle} control: {html}"
    );
    assert!(
        invalid[0].contains(&format!("aria-describedby=\"{error_id}\"")),
        "{needle} must be described by #{error_id}: {}",
        invalid[0]
    );
}

/// The inline error element with `id`, up to its closing tag.
fn error_element<'a>(html: &'a str, id: &str) -> &'a str {
    let needle = format!("id=\"{id}\"");
    let at = html
        .find(&needle)
        .unwrap_or_else(|| panic!("missing error element #{id}: {html}"));
    let end = at + html[at..].find("</p>").unwrap();
    let start = html[..at].rfind('<').unwrap();
    assert!(
        html[start..at].contains("class=\"field-error\""),
        "#{id} must be styled as a field error"
    );
    &html[at..end]
}

/// The top-of-page validation summary.
fn error_summary(html: &str) -> &str {
    let at = html
        .find("data-error-summary")
        .unwrap_or_else(|| panic!("missing error summary: {html}"));
    let end = at + html[at..].find("</div>").unwrap();
    &html[at..end]
}

fn assert_summary_has_no_storage_paths(summary: &str) {
    assert!(!summary.contains("row "), "no `row N` wording: {summary}");
    assert!(
        !summary.contains("ai.profiles"),
        "no raw storage paths: {summary}"
    );
    assert!(!summary.contains("form."), "no raw form paths: {summary}");
}

/// Store validation flags the appended row (`ai.profiles[1].name`); that is
/// the create form's row, so the error belongs beside its Name input.
#[tokio::test]
async fn add_duplicate_name_is_flagged_beside_the_name_input() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=a&provider=open_router&model=kept-model&timeout=30&key=k",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/add",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    assert_invalid_control(&html, "name=\"name\"", "profile-new-name-error");
    assert!(
        error_element(&html, "profile-new-name-error").contains("duplicate profile name"),
        "{html}"
    );
    assert!(
        html.contains("value=\"kept-model\""),
        "rejected values stay"
    );
    let summary = error_summary(&html);
    assert!(summary.contains("Fix the highlighted fields"), "{summary}");
    assert!(
        !summary.contains("<li>"),
        "inline errors are not repeated: {summary}"
    );
    assert_summary_has_no_storage_paths(summary);
    assert_eq!(state.settings.load().ai.profiles.len(), 1);
}

/// An invalid tier on the edited row lands on the active provider group's
/// tier select, inside an open Advanced section.
#[tokio::test]
async fn edit_invalid_service_tier_is_flagged_beside_the_tier_select() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(
        &state,
        vec![profile("other"), profile("a")],
        "other",
    ))
    .await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=a&provider=open_router&model=m&timeout=30&key=k&service_tier=turbo",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/a/edit",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    // Every provider group ships a tier select; only the active one is
    // flagged.
    assert_invalid_control(
        &html,
        "name=\"service_tier\"",
        "profile-edit-service_tier-error",
    );
    assert!(
        error_element(&html, "profile-edit-service_tier-error").contains("must be one of"),
        "{html}"
    );
    let details = tags_containing(&html, "class=\"profile-advanced\"");
    assert_eq!(details.len(), 1, "{html}");
    assert!(details[0].contains("open"), "Advanced must render open");
    assert!(html.contains("action=\"/ai-profiles/a/edit\""), "{html}");
    assert_summary_has_no_storage_paths(error_summary(&html));
    assert_eq!(state.settings.load().ai.profiles[1].service_tier, None);
}

/// Renaming onto an existing name is flagged on the *other* row (the later
/// duplicate); the fix still belongs in this form's Name input.
#[tokio::test]
async fn edit_rename_onto_an_existing_name_is_flagged_beside_the_name_input() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a"), profile("b")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=b&provider=open_router&model=m&timeout=30&key=k",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/a/edit",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    assert_invalid_control(&html, "name=\"name\"", "profile-edit-name-error");
    assert!(html.contains("action=\"/ai-profiles/a/edit\""), "{html}");
    let summary = error_summary(&html);
    assert!(!summary.contains("<li>"), "{summary}");
    assert_summary_has_no_storage_paths(summary);
}

/// `ProfileForm::to_profile` rejects a non-numeric timeout (`form.timeout`)
/// before the store runs; it still renders beside the Timeout input.
#[tokio::test]
async fn add_non_numeric_timeout_is_flagged_beside_the_timeout_input() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=fresh&provider=open_router&model=m&timeout=soon&key=k",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/add",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    assert_invalid_control(&html, "name=\"timeout\"", "profile-new-timeout-error");
    assert!(
        error_element(&html, "profile-new-timeout-error").contains("whole number of seconds"),
        "{html}"
    );
    assert!(html.contains("value=\"soon\""), "rejected values stay");
    let details = tags_containing(&html, "class=\"profile-advanced\"");
    assert!(details[0].contains("open"), "Advanced must render open");
    assert_summary_has_no_storage_paths(error_summary(&html));
    assert_eq!(state.settings.load().ai.profiles.len(), 1);
}

/// The connection-test route re-renders through the same path: a form-parse
/// error shows inline on the edit form, which keeps its original target.
#[tokio::test]
async fn test_connection_form_error_is_flagged_inline() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&_edit_target=a&name=a&provider=open_ai&model=gpt-5&timeout=30",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/test",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    assert_invalid_control(&html, "name=\"key\"", "profile-edit-key-error");
    assert!(html.contains("action=\"/ai-profiles/a/edit\""), "{html}");
    assert_summary_has_no_storage_paths(error_summary(&html));
}

/// A delete blocked by agent pointers has no form control: the summary names
/// the profile in plain words and nothing is marked invalid.
#[tokio::test]
async fn delete_in_use_profile_names_it_in_the_summary() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(
        &state,
        vec![profile("spare"), profile("busy-profile")],
        "busy-profile",
    ))
    .await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!("_csrf={}", urlencoding::encode(&csrf));
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/busy-profile/delete",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    let summary = error_summary(&html);
    assert!(summary.contains("busy-profile"), "{summary}");
    assert!(summary.contains("Aurora"), "{summary}");
    assert!(!summary.contains("Fix the highlighted fields"), "{summary}");
    assert_summary_has_no_storage_paths(summary);
    assert!(!html.contains("aria-invalid"), "{html}");
    assert_eq!(state.settings.load().ai.profiles.len(), 2);
}

/// An error for a control the selected provider does not render (Ollama has
/// no reasoning effort) must still reach the user, via the summary.
#[tokio::test]
async fn error_for_a_hidden_control_falls_back_to_the_summary() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    Box::pin(seed_profiles(&state, vec![profile("a")], "a")).await;
    let (sid, cc, csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let body = format!(
        "_csrf={}&name=a&provider=ollama&model=qwen3%3A8b&timeout=30&reasoning_effort=high",
        urlencoding::encode(&csrf)
    );
    let res = post_form(
        build_router(state.clone()),
        &sid,
        &cc,
        "/ai-profiles/a/edit",
        body,
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    let summary = error_summary(&html);
    assert!(
        summary.contains("Reasoning effort") && summary.contains("no reasoning effort setting"),
        "{summary}"
    );
    assert_summary_has_no_storage_paths(summary);
    assert!(!html.contains("aria-invalid"), "{html}");
}
