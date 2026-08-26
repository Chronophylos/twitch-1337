//! Integration tests for /ai-profiles CRUD (owner-gated — issue I1: a
//! profile carries `base_url`/`key`, the same credential surface that was
//! owner-only on `main`).

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_core::settings::{Actor, AiProfile, AiProvider, overrides::SettingsOverrides};
use twitch_1337_web::build_router;

mod helpers;
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
                    active_profile: Some(active.to_owned()),
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
async fn activate_switches_the_active_profile() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_profiles(&state, vec![profile("a"), profile("b")], "a").await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "owner", twitch_1337_web::auth::Role::Owner);
    let app = build_router(state.clone());

    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/b/activate")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.settings.load().ai.active_profile, "b");
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

#[tokio::test]
async fn delete_refuses_to_remove_the_active_profile() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_profiles(&state, vec![profile("a"), profile("b")], "b").await;
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
    assert_eq!(res.status(), StatusCode::OK); // re-rendered with an error
    let settings = state.settings.load();
    assert_eq!(settings.ai.profiles.len(), 2);
    assert_eq!(settings.ai.active_profile, "b");
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
async fn mod_cannot_activate_a_profile() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_profiles(&state, vec![profile("a"), profile("b")], "a").await;
    let (sid, csrf_cookie, bare_csrf) =
        insert_session_as(&state, "999", "mod", twitch_1337_web::auth::Role::Mod);
    let app = build_router(state.clone());

    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/ai-profiles/b/activate")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        state.settings.load().ai.active_profile,
        "a",
        "a mod's activate must not take effect"
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
async fn edit_renaming_active_profile_carries_the_pointer() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_profiles(&state, vec![profile("old")], "old").await;
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
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let settings = state.settings.load();
    assert_eq!(settings.ai.profiles.len(), 1);
    assert_eq!(settings.ai.profiles[0].name, "renamed");
    assert_eq!(settings.ai.active_profile, "renamed");
}

#[tokio::test]
async fn delete_clears_dangling_sub_agent_pointers() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_profiles(&state, vec![profile("a"), profile("b")], "a").await;
    state
        .settings_store
        .apply(
            SettingsOverrides {
                ai: twitch_1337_core::settings::AiOverrides {
                    dreamer: twitch_1337_core::settings::overrides::AiDreamerOverrides {
                        profile: Some(Some("b".into())),
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
        .expect("seed dreamer pointer");
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
    assert_eq!(settings.ai.dreamer.profile, None);
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
    assert_eq!(settings.ai.active_profile, "old");

    // Step 2: resubmit the corrected edit to the action the page actually
    // rendered (still "/ai-profiles/old/edit") — the rename must land, and
    // since "old" was the active profile, the pointer must follow it.
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
    assert_eq!(settings.ai.active_profile, "renamed");
}
