//! Integration tests for /ai-profiles CRUD (mod-gated).

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_core::settings::{Actor, AiProfile, AiProvider, overrides::SettingsOverrides};
use twitch_1337_web::build_router;

mod helpers;
use helpers::{
    FakeHelix, build_state_with_all_dirs, cookie_header, insert_session, insert_session_as,
    install_crypto,
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
        key: None,
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
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
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
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
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
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
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

#[tokio::test]
async fn the_page_is_mod_gated() {
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
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn add_rejects_an_effort_the_provider_does_not_support() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_profiles(&state, vec![profile("a")], "a").await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
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
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
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
async fn edit_renaming_active_profile_carries_the_pointer() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_profiles(&state, vec![profile("old")], "old").await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    let body = format!(
        "_csrf={csrf}&name=renamed&provider=open_router&model=some-model&timeout=30",
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
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
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
