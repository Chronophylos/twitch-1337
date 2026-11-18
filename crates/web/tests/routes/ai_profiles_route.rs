//! Integration tests for /ai-profiles CRUD (owner-gated — issue I1: a
//! profile carries `base_url`/`key`, the same credential surface that was
//! owner-only on `main`).

use std::{fs, sync::Arc};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_core::settings::{Actor, AiProfile, AiProvider, SettingsOverrides};
use twitch_1337_web::build_router;

use super::helpers;
use helpers::{
    FakeHelix, build_state_with_all_dirs, cookie_header, insert_session_as, install_crypto,
};

fn empty_helix() -> Arc<FakeHelix> {
    Arc::new(FakeHelix {
        moderators: vec!["999".into()],
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
    seed_profiles(&state, vec![profile("only")], "only").await;
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
    seed_profiles(&state, vec![profile("a"), profile("b")], "a").await;
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
    seed_profiles(&state, vec![profile("a")], "a").await;
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
    seed_profiles(&state, vec![profile("a")], "a").await;
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
    seed_profiles(&state, vec![profile("a")], "a").await;
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
    assert!(
        html.contains("form.key"),
        "validation must name the key field; got: {html}"
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
    seed_profiles(&state, vec![profile("old")], "old").await;
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
        seed_profiles(&state, vec![profile("a"), profile("b")], "a").await;
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
    seed_profiles(&state, vec![profile("a"), profile("b")], "a").await;
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
    seed_profiles(&state, vec![profile("old")], "old").await;
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
    seed_profiles(&state, vec![profile(name), profile("a")], "a").await;
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
    seed_profiles(&state, vec![profile("a")], "a").await;
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
    seed_profiles(&state, vec![profile("a")], "a").await;
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
    seed_profiles(&state, vec![profile("a"), decision("dec")], "a").await;
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
    seed_profiles(&state, vec![profile("a"), decision("dec")], "a").await;
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
    seed_profiles(&state, vec![profile("a"), decision("dec")], "a").await;
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
    seed_profiles(&state, vec![profile("a"), decision("dec")], "a").await;
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
