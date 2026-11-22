//! Integration tests for mod-gated `/emotes*` browse routes.

use super::helpers;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use helpers::{
    FakeHelix, build_state_with_dirs, cookie_header, insert_session, insert_session_as,
    install_crypto,
};
use tempfile::TempDir;
use tower::ServiceExt as _;
use twitch_1337_core::emote_tracker::{
    DB_FILENAME, EmoteTrackerHandle, MeaningField, NewSetEvent, NewUsageEvent, SetEventKind,
};
use twitch_1337_web::auth::require_mod;
use twitch_1337_web::routes::emotes;

fn mod_helix() -> Arc<FakeHelix> {
    Arc::new(FakeHelix {
        users: Default::default(),
    })
}

fn app(state: twitch_1337_web::WebState) -> Router {
    Router::new()
        .merge(emotes::router())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_mod,
        ))
        .with_state(state)
        .layer(tower_cookies::CookieManagerLayer::new())
}

async fn body_string(res: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 128 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn seeded_tracker() -> (Arc<EmoteTrackerHandle>, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(DB_FILENAME);
    let h = EmoteTrackerHandle::open(&path).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaa";
    h.upsert_emote(id, "Kappa", true, 1).await.unwrap();
    h.insert_set_event(NewSetEvent {
        at: 100,
        kind: SetEventKind::Bootstrap,
        emote_id: Some(id.into()),
        old_name: None,
        new_name: Some("Kappa".into()),
        actor_7tv_id: None,
        actor_username: None,
        actor_display_name: None,
        gap_from: None,
        gap_to: None,
        requester_login: None,
        requester_twitch_id: None,
        via: None,
        attribution_method: None,
    })
    .await
    .unwrap();
    h.insert_usage_events(vec![
        NewUsageEvent {
            at: 200,
            emote_id: id.into(),
            twitch_user_id: "1".into(),
            login: Some("alice".into()),
            flagged: false,
        },
        NewUsageEvent {
            at: 201,
            emote_id: id.into(),
            twitch_user_id: "2".into(),
            login: Some("supibot".into()),
            flagged: true,
        },
    ])
    .await
    .unwrap();
    (Arc::new(h), dir)
}

/// No tracker (feature off) degrades to a rendered page, not an error.
#[tokio::test]
async fn emotes_catalog_renders_without_tracker() {
    install_crypto();
    let (state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/emotes")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn emotes_audit_lists_bootstrap_for_mod() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let req = Request::builder()
        .uri("/emotes/audit")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("Kappa"), "got {html}");
    assert!(html.contains("bootstrap"), "got {html}");
}

#[tokio::test]
async fn emotes_usage_hides_flagged_by_default() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");

    let req = Request::builder()
        .uri("/emotes/usage")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("alice"), "got {html}");
    assert!(
        !html.contains("supibot"),
        "flagged login must be hidden by default; got {html}"
    );

    let req = Request::builder()
        .uri("/emotes/usage?flagged=1")
        .method(Method::GET)
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app(state).oneshot(req).await.unwrap();
    let html = body_string(res).await;
    assert!(html.contains("alice"), "got {html}");
    assert!(html.contains("supibot"), "got {html}");
}

/// 250 unflagged events, three per millisecond (like one chat message with
/// three emotes), each from a distinct login `u<i>`.
async fn tracker_with_250_events() -> (Arc<EmoteTrackerHandle>, TempDir) {
    let dir = TempDir::new().unwrap();
    let h = EmoteTrackerHandle::open(&dir.path().join(DB_FILENAME)).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaa";
    h.upsert_emote(id, "Kappa", true, 1).await.unwrap();
    let events = (0..250)
        .map(|i| NewUsageEvent {
            at: 1_000 + i / 3,
            emote_id: id.into(),
            twitch_user_id: i.to_string(),
            login: Some(format!("u{i}")),
            flagged: false,
        })
        .collect();
    h.insert_usage_events(events).await.unwrap();
    (Arc::new(h), dir)
}

async fn big_body(res: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 4 * 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn usage_logins(html: &str) -> Vec<String> {
    html.split(r#"data-label="User">"#)
        .skip(1)
        .map(|s| s.split('<').next().unwrap().to_owned())
        .collect()
}

/// The "Load more" URL from the page's `#usage-more` block.
fn next_href(html: &str) -> Option<String> {
    let more = &html[html.find(r#"id="usage-more""#)?..];
    let start = more.find(r#"hx-get=""#)? + r#"hx-get=""#.len();
    let href = &more[start..start + more[start..].find('"')?];
    // askama escapes `&` in attributes as `&#38;`; the browser decodes it.
    Some(href.replace("&#38;", "&"))
}

fn get(uri: &str, sid: &str, csrf: &str, htmx: bool) -> Request<Body> {
    let mut req = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie_header(sid, csrf));
    if htmx {
        req = req.header("HX-Request", "true");
    }
    req.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn emotes_usage_load_more_pages_by_cursor() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = tracker_with_250_events().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");

    // First page: full page, first 200 rows, told so.
    let res = app(state.clone())
        .oneshot(get("/emotes/usage?login=u", &sid, &csrf, false))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = big_body(res).await;
    assert!(html.contains("<html"), "full page");
    let first = usage_logins(&html);
    assert_eq!(first.len(), 200);
    assert!(html.contains("Showing 200 of 250"), "status line");
    assert!(html.contains("(250)"), "heading shows the total");
    let href = next_href(&html).expect("load more button");
    assert!(
        href.starts_with("/emotes/usage?login=u&"),
        "keeps filters: {href}"
    );
    assert!(href.contains("before=") && href.contains("before_id=") && href.contains("shown=200"));

    // htmx "Load more": rows to append + status/button out of band.
    let res = app(state.clone())
        .oneshot(get(&href, &sid, &csrf, true))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::VARY).unwrap(), "HX-Request");
    let frag = big_body(res).await;
    assert!(!frag.contains("<html"), "fragment only");
    assert!(!frag.contains("class=\"thead\""), "rows only");
    let second = usage_logins(&frag);
    assert_eq!(second.len(), 50);
    assert!(
        frag.contains(r#"id="usage-more" hx-swap-oob="true""#),
        "status swapped out of band; got {frag}"
    );
    assert!(frag.contains("Showing all 250"), "got {frag}");
    assert!(next_href(&frag).is_none(), "nothing left to load");

    let mut all: Vec<String> = first.into_iter().chain(second).collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 250, "every event exactly once across pages");

    // The same cursor URL without htmx is a normal full page.
    let res = app(state)
        .oneshot(get(&href, &sid, &csrf, false))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = big_body(res).await;
    assert!(html.contains("<html"), "full page");
    assert_eq!(usage_logins(&html).len(), 50);
    assert!(html.contains("Showing all 250"));
}

fn form_post(uri: &str, sid: &str, csrf: &str, form: String) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .method(Method::POST)
        .header(header::COOKIE, cookie_header(sid, csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap()
}

#[tokio::test]
async fn meanings_are_mod_gated_and_render_image_badges_and_filters() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    tracker
        .upsert_emote_meaning(
            "aaaaaaaaaaaaaaaaaaaaaaaa",
            "grinsendes Gesicht",
            None,
            None,
            None,
            1,
        )
        .await
        .unwrap();
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _) = insert_session_as(
        &state,
        "viewer",
        "viewer",
        twitch_1337_web::auth::Role::Viewer,
    );
    let denied = app(state.clone())
        .oneshot(get("/emotes/meanings", &sid, &csrf, false))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let (sid, csrf, _) = insert_session(&state, "42", "admin");
    let response = app(state)
        .oneshot(get(
            "/emotes/meanings?filter=no_context",
            &sid,
            &csrf,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("https://cdn.7tv.app/emote/aaaaaaaaaaaaaaaaaaaaaaaa/2x.webp"));
    assert!(html.contains("auto"));
    assert!(html.contains("Kappa"));
    assert!(html.contains("name=\"original_appearance\" value=\"grinsendes Gesicht\""));
}

#[tokio::test]
async fn meaning_edit_requires_csrf_marks_changed_field_and_unlock_restores_auto() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    tracker
        .upsert_emote_meaning(
            "aaaaaaaaaaaaaaaaaaaaaaaa",
            "alt",
            Some("auto"),
            None,
            None,
            1,
        )
        .await
        .unwrap();
    state.emote_tracker = Some(tracker.clone());
    let (sid, csrf, bare) = insert_session(&state, "42", "admin");
    let uri = "/emotes/meanings/aaaaaaaaaaaaaaaaaaaaaaaa/edit";
    let missing_csrf = app(state.clone())
        .oneshot(form_post(
            uri,
            &sid,
            &csrf,
            "appearance=alt&meaning=mod&usage=&avoid=".into(),
        ))
        .await
        .unwrap();
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);

    let saved = app(state.clone())
        .oneshot(form_post(
            uri,
            &sid,
            &csrf,
            format!(
                "_csrf={bare}&original_appearance=alt&original_meaning=auto&original_usage=&original_avoid=&appearance=alt&meaning=mod&usage=&avoid="
            ),
        ))
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    let row = tracker
        .get_emote_meaning("aaaaaaaaaaaaaaaaaaaaaaaa")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.meaning.as_deref(), Some("mod"));
    assert!(row.curated.contains(MeaningField::Meaning));
    assert!(!row.curated.contains(MeaningField::Appearance));

    let unlocked = app(state)
        .oneshot(form_post(
            "/emotes/meanings/aaaaaaaaaaaaaaaaaaaaaaaa/unlock",
            &sid,
            &csrf,
            format!("_csrf={bare}&field=meaning"),
        ))
        .await
        .unwrap();
    assert_eq!(unlocked.status(), StatusCode::SEE_OTHER);
    let row = tracker
        .get_emote_meaning("aaaaaaaaaaaaaaaaaaaaaaaa")
        .await
        .unwrap()
        .unwrap();
    assert!(!row.curated.contains(MeaningField::Meaning));
}

#[tokio::test]
async fn meaning_edit_preserves_fields_generated_after_render() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    let id = "aaaaaaaaaaaaaaaaaaaaaaaa";
    tracker
        .upsert_emote_meaning(id, "alt", Some("auto"), None, None, 1)
        .await
        .unwrap();
    state.emote_tracker = Some(tracker.clone());
    let (sid, csrf, bare) = insert_session(&state, "42", "admin");

    tracker
        .upsert_emote_meaning(
            id,
            "generated-face",
            Some("generated-meaning"),
            Some("generated-usage"),
            Some("generated-avoid"),
            2,
        )
        .await
        .unwrap();
    let saved = app(state)
        .oneshot(form_post(
            "/emotes/meanings/aaaaaaaaaaaaaaaaaaaaaaaa/edit",
            &sid,
            &csrf,
            format!(
                "_csrf={bare}&original_appearance=alt&original_meaning=auto&original_usage=&original_avoid=&appearance=alt&meaning=mod&usage=&avoid="
            ),
        ))
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);

    let row = tracker.get_emote_meaning(id).await.unwrap().unwrap();
    assert_eq!(row.appearance, "generated-face");
    assert_eq!(row.meaning.as_deref(), Some("mod"));
    assert_eq!(row.usage.as_deref(), Some("generated-usage"));
    assert_eq!(row.avoid.as_deref(), Some("generated-avoid"));
    assert!(row.curated.contains(MeaningField::Meaning));
    assert!(!row.curated.contains(MeaningField::Appearance));
    assert!(!row.curated.contains(MeaningField::Usage));
    assert!(!row.curated.contains(MeaningField::Avoid));
}

#[tokio::test]
async fn meaning_edit_after_first_generation_preserves_untouched_no_row_fields() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    let id = "aaaaaaaaaaaaaaaaaaaaaaaa";
    state.emote_tracker = Some(tracker.clone());
    let (sid, csrf, bare) = insert_session(&state, "42", "admin");

    tracker
        .upsert_emote_meaning(
            id,
            "generated-face",
            Some("generated-meaning"),
            Some("generated-usage"),
            Some("generated-avoid"),
            2,
        )
        .await
        .unwrap();
    let saved = app(state)
        .oneshot(form_post(
            "/emotes/meanings/aaaaaaaaaaaaaaaaaaaaaaaa/edit",
            &sid,
            &csrf,
            format!(
                "_csrf={bare}&original_appearance=&original_meaning=&original_usage=&original_avoid=&appearance=mod-face&meaning=mod-meaning&usage=&avoid="
            ),
        ))
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);

    let row = tracker.get_emote_meaning(id).await.unwrap().unwrap();
    assert_eq!(row.appearance, "mod-face");
    assert_eq!(row.meaning.as_deref(), Some("mod-meaning"));
    assert_eq!(row.usage.as_deref(), Some("generated-usage"));
    assert_eq!(row.avoid.as_deref(), Some("generated-avoid"));
    assert!(row.curated.contains(MeaningField::Appearance));
    assert!(row.curated.contains(MeaningField::Meaning));
    assert!(!row.curated.contains(MeaningField::Usage));
    assert!(!row.curated.contains(MeaningField::Avoid));
}

#[tokio::test]
async fn meanings_run_now_is_owner_only() {
    install_crypto();
    let (state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (sid, csrf, bare) = insert_session(&state, "42", "admin");
    let denied = app(state.clone())
        .oneshot(form_post(
            "/emotes/meanings/run",
            &sid,
            &csrf,
            format!("_csrf={bare}"),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn emotes_usage_without_more_shows_all() {
    install_crypto();
    let (mut state, _td_pings, _td_mem) = build_state_with_dirs(mod_helix()).await;
    let (tracker, _db_dir) = seeded_tracker().await;
    state.emote_tracker = Some(tracker);
    let (sid, csrf, _bare) = insert_session(&state, "42", "admin");
    let res = app(state)
        .oneshot(get("/emotes/usage", &sid, &csrf, false))
        .await
        .unwrap();
    let html = big_body(res).await;
    assert!(html.contains("Showing all 1"), "got {html}");
    assert!(next_href(&html).is_none(), "no load more for a short list");
}
