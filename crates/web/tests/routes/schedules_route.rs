//! Integration tests for /schedules CRUD (mod-gated).

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_core::schedule::{Schedule, Trigger, WeekdaySet};
use twitch_1337_core::settings::{Actor, overrides::SettingsOverrides};
use twitch_1337_web::build_router;

use super::helpers;
use helpers::{
    FakeHelix, build_state_with_all_dirs, cookie_header, insert_session, install_crypto,
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

/// Apply schedules via the store so we can seed pre-test state without
/// going through the dashboard.
async fn seed_schedules(state: &twitch_1337_web::WebState, rows: Vec<Schedule>) {
    state
        .settings_store
        .apply(
            SettingsOverrides {
                schedules: Some(rows),
                ..Default::default()
            },
            Actor {
                user_id: "test".into(),
                user_login: "test".into(),
            },
        )
        .await
        .expect("seed schedules");
}

fn interval_schedule(name: &str, message: &str) -> Schedule {
    Schedule {
        ping: None,
        name: name.into(),
        message: message.into(),
        trigger: Trigger::Interval {
            every: Duration::from_secs(3600),
            days: WeekdaySet::default(),
            active_from: None,
            active_to: None,
        },
        start_date: None,
        end_date: None,
        enabled: true,
    }
}

/// Build a URL-encoded form body for a new/updated hourly interval schedule.
fn interval_form(csrf: &str, name: &str, message: &str) -> String {
    interval_form_every(csrf, name, message, "1", "0")
}

/// Interval form body with explicit hours + minutes inputs.
fn interval_form_every(
    csrf: &str,
    name: &str,
    message: &str,
    hours: &str,
    minutes: &str,
) -> String {
    format!(
        "_csrf={csrf}&name={name}&message={message}&kind=interval&interval_hours={hours}&interval_minutes={minutes}&interval_active_from=&interval_active_to=&start_date=&end_date=&enabled=true",
        csrf = urlencoding::encode(csrf),
    )
}

fn interval_secs(state: &twitch_1337_web::WebState, name: &str) -> u64 {
    let rows = state.settings.load().schedules.clone();
    match &rows
        .iter()
        .find(|s| s.name == name)
        .expect("row exists")
        .trigger
    {
        Trigger::Interval { every, .. } => every.as_secs(),
        other => panic!("expected interval trigger, got {other:?}"),
    }
}

/// The interval is two number inputs (hours, minutes). Create stores their
/// sum, the edit form splits it back out, and posting that form unchanged
/// keeps the same interval.
#[tokio::test]
async fn interval_inputs_round_trip_through_create_and_edit() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());
    let post = |uri: &str, body: String| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    };

    let res = app
        .clone()
        .oneshot(post(
            "/schedules/add",
            interval_form_every(&bare_csrf, "tick", "hi", "1", "30"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(interval_secs(&state, "tick"), 5400);

    let req = Request::builder()
        .uri("/schedules?edit=tick")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let html = body_string(app.clone().oneshot(req).await.unwrap()).await;
    assert!(
        html.contains(r#"name="interval_hours" min="0" max="24""#),
        "hours input; got {html}"
    );
    assert!(
        !html.contains(r#"name="interval_every""#),
        "no hh:mm text input"
    );
    assert!(
        !html.contains(r#"lang="de""#),
        "no lang on date/time inputs"
    );
    let value_of = |name: &str| {
        let at = html
            .find(&format!(r#"name="{name}""#))
            .unwrap_or_else(|| panic!("no {name} input"));
        let rest = &html[at..];
        let v = &rest[rest.find(r#"value=""#).unwrap() + 7..];
        v[..v.find('"').unwrap()].to_owned()
    };
    let (hours, minutes) = (value_of("interval_hours"), value_of("interval_minutes"));
    assert_eq!((hours.as_str(), minutes.as_str()), ("1", "30"));

    let res = app
        .clone()
        .oneshot(post(
            "/schedules/tick/edit",
            interval_form_every(&bare_csrf, "tick", "hi", &hours, &minutes),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        interval_secs(&state, "tick"),
        5400,
        "edit keeps the interval"
    );

    // Out-of-range minutes fail validation and leave the row untouched.
    let res = app
        .oneshot(post(
            "/schedules/tick/edit",
            interval_form_every(&bare_csrf, "tick", "hi", "1", "75"),
        ))
        .await
        .unwrap();
    let html = body_string(res).await;
    assert!(
        html.contains("must be 0–59"),
        "minutes error shown; got {html}"
    );
    assert_eq!(interval_secs(&state, "tick"), 5400);
}

#[tokio::test]
async fn add_schedule_persists_and_renders() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    let body = interval_form(&bare_csrf, "noon", "midday");
    let req = Request::builder()
        .method("POST")
        .uri("/schedules/add")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let listed = {
        let req = Request::builder()
            .uri("/schedules")
            .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        body_string(res).await
    };
    assert!(listed.contains("noon"), "row must appear in list");
    let v = state.settings.load().schedules.clone();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].name, "noon");
}

/// A calendar schedule with exactly ONE weekday selected sends a single
/// `calendar_days=Thu` field. Plain `axum::Form` (serde_urlencoded) rejected
/// that as "invalid type: string, expected a sequence"; the `axum_extra` Form
/// extractor parses a lone occurrence into a 1-element Vec.
#[tokio::test]
async fn add_calendar_schedule_with_single_weekday() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    let body = format!(
        "_csrf={csrf}&name=thurs&message=hi&kind=calendar&calendar_at=13%3A37&calendar_days=Thu&start_date=&end_date=&enabled=true",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/schedules/add")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let v = state.settings.load().schedules.clone();
    assert_eq!(v.len(), 1);
    match &v[0].trigger {
        Trigger::Calendar { days, .. } => {
            assert!(
                days.contains(jiff::civil::Weekday::Thursday),
                "Thu must be set"
            );
            assert!(
                !days.contains(jiff::civil::Weekday::Monday),
                "only Thu selected"
            );
        }
        other => panic!("expected calendar trigger, got {other:?}"),
    }
}

#[tokio::test]
async fn edit_schedule_renames() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(&state, vec![interval_schedule("old", "hi")]).await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    let body = interval_form(&bare_csrf, "new", "hi");
    let req = Request::builder()
        .method("POST")
        .uri("/schedules/old/edit")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let v = state.settings.load().schedules.clone();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].name, "new");
}

#[tokio::test]
async fn edit_duplicate_name_returns_error_without_persisting() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(
        &state,
        vec![interval_schedule("a", "hi"), interval_schedule("b", "hi")],
    )
    .await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    let body = interval_form(&bare_csrf, "a", "hi");
    let req = Request::builder()
        .method("POST")
        .uri("/schedules/b/edit")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    // Validation error renders the list with errors; status is 200, not 303.
    assert_eq!(res.status().as_u16(), 200);
    let v = state.settings.load().schedules.clone();
    assert_eq!(v.len(), 2, "no row may be renamed on validation failure");
    assert!(v.iter().any(|s| s.name == "a"));
    assert!(v.iter().any(|s| s.name == "b"));
}

/// A create that fails store-level validation (duplicate name) must restore
/// the editable form with the user's input — NOT render an interactive card
/// for the rejected, never-persisted row (clicking such a card targeted a
/// nonexistent schedule and made the whole thing vanish).
#[tokio::test]
async fn create_duplicate_name_restores_form_without_phantom_card() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(&state, vec![interval_schedule("dup", "original")]).await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    let body = interval_form(&bare_csrf, "dup", "keepme");
    let req = Request::builder()
        .method("POST")
        .uri("/schedules/add")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status().as_u16(),
        200,
        "validation error re-renders, not 303"
    );

    let v = state.settings.load().schedules.clone();
    assert_eq!(v.len(), 1, "rejected row must not persist");

    let html = body_string(res).await;
    // Form restored with the submitted message so the user can fix the name.
    assert!(
        html.contains("action=\"/schedules/add\""),
        "new-schedule form must be restored"
    );
    assert!(html.contains("keepme"), "submitted input must be preserved");
    // Exactly one card for "dup" — the persisted one. A phantom card for the
    // rejected attempt would push this to two.
    assert_eq!(
        html.matches(r#"action="/schedules/dup/toggle""#).count(),
        1,
        "no phantom card for the unsaved row"
    );
    // The error still surfaces, but attributed to the form (global), not a
    // phantom "row 1" that exceeds the single persisted card.
    assert!(html.contains("Validation failed"), "error must be shown");
    assert!(
        !html.contains("row 1"),
        "no phantom row reference for the unsaved attempt"
    );
}

#[tokio::test]
async fn delete_schedule_removes_row() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(&state, vec![interval_schedule("gone", "x")]).await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    let body = format!("_csrf={csrf}", csrf = urlencoding::encode(&bare_csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/schedules/gone/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let v = state.settings.load().schedules.clone();
    assert!(v.is_empty());
}

fn card_post(
    uri: &str,
    sid: &str,
    csrf_cookie: &str,
    bare_csrf: &str,
    htmx: bool,
) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie_header(sid, csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if htmx {
        req = req.header("HX-Request", "true");
    }
    req.body(Body::from(format!(
        "_csrf={}",
        urlencoding::encode(bare_csrf)
    )))
    .unwrap()
}

/// htmx toggle answers with just the re-rendered card plus the
/// out-of-band stat strip, never the full page.
#[tokio::test]
async fn toggle_via_htmx_returns_card_and_oob_stats() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(
        &state,
        vec![
            interval_schedule("flip", "x"),
            interval_schedule("other", "y"),
        ],
    )
    .await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let res = build_router(state.clone())
        .oneshot(card_post(
            "/schedules/flip/toggle",
            &sid,
            &csrf_cookie,
            &bare_csrf,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(!html.contains("<html"), "fragment only; got {html}");
    assert_eq!(
        html.matches("<article").count(),
        1,
        "only the toggled card; got {html}"
    );
    assert!(
        html.contains(r#"<article class="schedule-card paused">"#),
        "card re-rendered as paused; got {html}"
    );
    assert!(html.contains("/schedules/flip/toggle"), "got {html}");
    assert!(
        html.contains(r#"id="schedule-stats" hx-swap-oob="true""#),
        "stat strip out of band; got {html}"
    );
    assert!(
        html.contains(r#"id="schedule-stat-active">1</div>"#),
        "active count updated; got {html}"
    );
    let rows = state.settings.load().schedules.clone();
    assert!(!rows.iter().find(|s| s.name == "flip").unwrap().enabled);
}

#[tokio::test]
async fn toggle_without_htmx_redirects() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(&state, vec![interval_schedule("flip", "x")]).await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let res = build_router(state.clone())
        .oneshot(card_post(
            "/schedules/flip/toggle",
            &sid,
            &csrf_cookie,
            &bare_csrf,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/schedules");
    assert!(!state.settings.load().schedules[0].enabled);
}

/// htmx delete answers with no card (the `outerHTML` swap removes it) and
/// the out-of-band counts.
#[tokio::test]
async fn delete_via_htmx_returns_only_oob_stats() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(&state, vec![interval_schedule("gone", "x")]).await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let res = build_router(state.clone())
        .oneshot(card_post(
            "/schedules/gone/delete",
            &sid,
            &csrf_cookie,
            &bare_csrf,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(!html.contains("<html"), "fragment only; got {html}");
    assert!(
        !html.contains("<article"),
        "no card after delete; got {html}"
    );
    assert!(
        html.contains(r#"<span class="count" id="schedules-total" hx-swap-oob="true">(0)</span>"#),
        "heading count out of band; got {html}"
    );
    assert!(
        html.contains(r#"id="schedule-stats" hx-swap-oob="true""#),
        "got {html}"
    );
    assert!(state.settings.load().schedules.is_empty());
}

/// The page renders the ids the out-of-band swaps target, and delete asks first.
#[tokio::test]
async fn list_wires_cards_for_htmx() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(&state, vec![interval_schedule("flip", "x")]).await;
    let (sid, csrf_cookie, _bare) = insert_session(&state, "999", "mod");
    let req = Request::builder()
        .uri("/schedules")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .body(Body::empty())
        .unwrap();
    let html = body_string(build_router(state).oneshot(req).await.unwrap()).await;
    assert!(
        html.contains("hx-confirm="),
        "delete asks first; got {html}"
    );
    assert!(html.contains(r#"id="schedule-stats""#));
    assert!(html.contains(r#"id="schedules-total""#));
    assert!(!html.contains("hx-swap-oob"), "the page itself is not oob");
}

#[tokio::test]
async fn edit_validation_failure_reopens_form_with_new_name() {
    install_crypto();
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(
        &state,
        vec![
            interval_schedule("keep", "hi"),
            interval_schedule("b", "hi"),
        ],
    )
    .await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    // Try to rename "b" to "keep" — duplicate name, validation fails.
    let body = interval_form(&bare_csrf, "keep", "hi");
    let req = Request::builder()
        .method("POST")
        .uri("/schedules/b/edit")
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let body = body_string(res).await;
    // The inline edit form should reopen for the NEW name ("keep"), not the
    // URL path's old name ("b"). The rendered template iterates the in-memory
    // `next` vec — `b` has been replaced with `keep` (validation failure does
    // not roll the vec back since it's a local).
    assert!(
        body.contains("/schedules/keep/edit"),
        "edit form for new name 'keep' must reopen on validation failure"
    );
}

/// Schedule validation rejects `/`, `%`, `#` and friends, but other names
/// still need percent-encoding in a path (`;`, `<`, non-ASCII). The rendered
/// edit action must round-trip through the router to the same row.
#[tokio::test]
async fn edit_form_action_round_trips_names_needing_encoding() {
    install_crypto();
    let name = "grüße;<x>";
    let (state, _td_p, _td_m, _td_s) = build_state_with_all_dirs(empty_helix()).await;
    seed_schedules(
        &state,
        vec![interval_schedule(name, "hi"), interval_schedule("a", "hi")],
    )
    .await;
    let (sid, csrf_cookie, bare_csrf) = insert_session(&state, "999", "mod");
    let app = build_router(state.clone());

    let req = Request::builder()
        .uri(format!("/schedules?edit={}", urlencoding::encode(name)))
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

    let req = Request::builder()
        .method("POST")
        .uri(&action)
        .header(header::COOKIE, cookie_header(&sid, &csrf_cookie))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(interval_form(&bare_csrf, "renamed", "hi")))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "POST {action}");
    let mut names: Vec<_> = state
        .settings
        .load()
        .schedules
        .iter()
        .map(|s| s.name.clone())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["a", "renamed"],
        "POST {action} must rename {name:?}"
    );
}
