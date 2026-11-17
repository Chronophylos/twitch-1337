//! Integration tests for `/pings` CRUD routes.
//!
//! Drives `build_router` directly via `tower::ServiceExt::oneshot`. Sessions
//! are pre-seeded so the require_mod middleware admits without an OAuth
//! round-trip; the FakeHelix client returns `is_moderator = true` for the
//! seeded user so the periodic mod-recheck inside `require_mod` doesn't
//! deny.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use twitch_1337_web::build_router;
use twitch_1337_web::helix::{HelixClient, HelixUser};

use super::helpers;
use helpers::{
    FakeHelix, build_state_with_ping_dir, cookie_header, insert_session, insert_session_as,
    install_crypto,
};

/// FakeHelix flavored to admit the test admin during periodic mod rechecks.
fn admin_helix(user_id: &str) -> Arc<dyn HelixClient> {
    let mut users = HashMap::new();
    users.insert(
        user_id.to_owned(),
        HelixUser {
            id: user_id.to_owned(),
            login: "admin".into(),
            display_name: "admin".into(),
            profile_image_url: None,
        },
    );
    Arc::new(FakeHelix {
        moderators: vec![user_id.to_owned()],
        users,
    })
}

/// Build (state, signed_sid, signed_csrf, bare_csrf, _td). `_td` keeps the
/// ping data dir alive while tests exercise PingManager's atomic save+rename.
/// Drop it to clean up.
///
/// `signed_csrf` is what the browser sends as the `tw1337_csrf` cookie after
/// signed-add. `bare_csrf` is the user-visible value for the `_csrf` form
/// field or `X-Csrf-Token` header (constant-time compared via
/// `crate::auth::csrf::verify`).
async fn authed_setup() -> (
    twitch_1337_web::WebState,
    String,
    String,
    String,
    tempfile::TempDir,
) {
    install_crypto();
    let user_id = "9001";
    let helix = admin_helix(user_id);
    let (state, td) = build_state_with_ping_dir(helix).await;
    let (sid, csrf, bare_csrf) = insert_session(&state, user_id, "admin");
    (state, sid, csrf, bare_csrf, td)
}

async fn body_string(res: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn list_renders_existing_pings() {
    let (state, sid, csrf, _bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "Hey {mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    let app = build_router(state);
    let req = Request::builder()
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.contains("team"),
        "list should contain ping name; got {html}"
    );
    assert!(
        html.contains("Hey {mentions}"),
        "list should show template; got {html}"
    );
    assert!(
        html.contains(r#"hx-target="closest .tr""#),
        "delete button must target the `.tr` div, not the absent <tr> element; got {html}"
    );
}

#[tokio::test]
async fn create_rejects_control_chars() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    let app = build_router(state.clone());
    let body = format!(
        "_csrf={csrf}&name=bad&template=oops%0Aboom",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(
        state.ping_actor.get_one("bad".into()).await.is_none(),
        "control-char ping must not persist"
    );
}

#[tokio::test]
async fn create_rejects_duplicate_name() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("dup".into(), "x {mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    let app = build_router(state.clone());
    // Case-insensitive dup check: try "DUP".
    let body = format!(
        "_csrf={csrf}&name=DUP&template=other%20{{mentions}}",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        state.ping_actor.snapshot().await.len(),
        1,
        "no second ping should be created"
    );
}

#[tokio::test]
async fn edit_round_trip() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "Hey {mentions}".into(), "admin".into(), None)
        .await
        .unwrap();

    // GET edit form — must contain the existing template.
    let app = build_router(state.clone());
    let req = Request::builder()
        .uri("/pings/team")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(
        html.contains("Hey {mentions}"),
        "edit form should pre-fill template; got {html}"
    );

    // POST update with a new template.
    let app = build_router(state.clone());
    let body = format!(
        "_csrf={csrf}&template=Updated%20{{mentions}}",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings/team")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let view = state.ping_actor.get_one("team".into()).await.unwrap();
    assert_eq!(view.template, "Updated {mentions}");
}

/// htmx delete: the main swap is empty (so `outerHTML` removes the row) and
/// every count the delete changed comes back as an out-of-band swap.
#[tokio::test]
async fn delete_via_htmx_returns_oob_counts() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("doomed".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    state
        .ping_actor
        .create_ping("kept".into(), "{mentions}".into(), "bob".into(), None)
        .await
        .unwrap();
    state
        .ping_actor
        .add_member("kept".into(), "alice".into())
        .await
        .unwrap();
    let app = build_router(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/pings/doomed/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header("X-Csrf-Token", bare_csrf.clone())
        .header("HX-Request", "true")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        !res.headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|c| c.to_str().unwrap_or_default().starts_with("tw1337_flash=")),
        "an htmx delete must not leave a flash for the next page load"
    );
    let html = body_string(res).await;
    assert!(!html.contains("<html"), "fragment only; got {html}");
    assert!(
        html.contains(r#"<span class="count" id="pings-total" hx-swap-oob="true">(1)</span>"#),
        "heading count; got {html}"
    );
    assert!(
        html.contains(r#"id="ping-stats" hx-swap-oob="true""#),
        "stat strip; got {html}"
    );
    for (key, n) in [("all", 1), ("mine", 0), ("joined", 0), ("fired", 0)] {
        let badge =
            format!(r#"<span class="chip-n" id="ping-chip-n-{key}" hx-swap-oob="true">{n}</span>"#);
        assert!(html.contains(&badge), "missing {badge}; got {html}");
    }
    // Every top-level element is out-of-band, so nothing lands in the row.
    assert!(!html.contains("class=\"tr\""), "no row markup; got {html}");
    assert!(
        state.ping_actor.get_one("doomed".into()).await.is_none(),
        "delete must remove the ping"
    );
}

/// Without `HX-Request` the same endpoint redirects back to the list.
#[tokio::test]
async fn delete_without_htmx_redirects_to_list() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("doomed".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    let app = build_router(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/pings/doomed/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header("X-Csrf-Token", bare_csrf.clone())
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/pings");
    assert!(state.ping_actor.get_one("doomed".into()).await.is_none());
}

/// The list renders the ids the out-of-band swaps target.
#[tokio::test]
async fn list_carries_oob_target_ids() {
    let (state, sid, csrf, _bare, _td) = authed_setup().await;
    let req = Request::builder()
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = build_router(state).oneshot(req).await.unwrap();
    let html = body_string(res).await;
    for id in [
        "pings-total",
        "ping-stats",
        "ping-chip-n-all",
        "ping-chip-n-mine",
        "ping-chip-n-joined",
        "ping-chip-n-fired",
    ] {
        assert!(html.contains(&format!("id=\"{id}\"")), "missing #{id}");
    }
    assert!(!html.contains("hx-swap-oob"), "the page itself is not oob");
}

#[tokio::test]
async fn delete_without_csrf_rejected() {
    let (state, sid, csrf, _bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("survives".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    let app = build_router(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/pings/survives/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        // No X-Csrf-Token header.
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(
        state.ping_actor.get_one("survives".into()).await.is_some(),
        "ping must persist when csrf header is absent",
    );
}

fn htmx_delete_without_session(current_url: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/pings/whatever/delete")
        .header("HX-Request", "true")
        .header("HX-Current-URL", current_url)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn htmx_delete_without_session_answers_hx_redirect() {
    let (state, _sid, _csrf, _bare_csrf, _td) = authed_setup().await;
    let app = build_router(state);
    let res = app
        .oneshot(htmx_delete_without_session(
            "http://localhost:8761/pings?filter=x",
        ))
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "htmx would follow a 303 and swap the login page into the row"
    );
    assert!(res.headers().get(header::LOCATION).is_none());
    assert_eq!(
        res.headers().get("HX-Redirect").unwrap(),
        "/login?next=%2Fpings%3Ffilter%3Dx",
        "next must be the page the user is on, not the delete endpoint"
    );
}

#[tokio::test]
async fn htmx_redirect_drops_unsafe_current_url() {
    let (state, _sid, _csrf, _bare_csrf, _td) = authed_setup().await;
    let app = build_router(state);
    let res = app
        .oneshot(htmx_delete_without_session(
            "http://localhost//evil.example/x",
        ))
        .await
        .unwrap();
    assert_eq!(res.headers().get("HX-Redirect").unwrap(), "/login");
}

#[tokio::test]
async fn csrf_mismatch_form_post_renders_error_page() {
    let (state, sid, csrf, _bare_csrf, _td) = authed_setup().await;
    let app = build_router(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("_csrf=wrong&name=nope&template=hi"))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let ct = res.headers().get(header::CONTENT_TYPE).unwrap();
    assert!(ct.to_str().unwrap().starts_with("text/html"), "got {ct:?}");
    let html = body_string(res).await;
    assert!(html.contains("Session expired"), "{html}");
    assert!(html.contains("data-history-back"), "missing back button");
    assert!(state.ping_actor.get_one("nope".into()).await.is_none());
}

#[tokio::test]
async fn csrf_mismatch_htmx_returns_plain_text_for_toast() {
    let (state, sid, csrf, _bare_csrf, _td) = authed_setup().await;
    let app = build_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/pings/whatever/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header("HX-Request", "true")
        .header("X-Csrf-Token", "wrong")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let ct = res.headers().get(header::CONTENT_TYPE).unwrap();
    assert!(ct.to_str().unwrap().starts_with("text/plain"), "got {ct:?}");
    assert_eq!(
        body_string(res).await,
        "Session expired, reload and try again"
    );
}

#[tokio::test]
async fn create_duplicate_renders_form_with_error_and_user_draft() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("foo".into(), "@user".into(), "admin".into(), None)
        .await
        .unwrap();
    let app = build_router(state);
    let body = format!(
        "_csrf={csrf}&name=foo&template=%40new-template-text",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let html = body_string(res).await;
    assert!(
        html.contains("@new-template-text"),
        "user draft must round-trip into the form; got {html}"
    );
    assert!(
        html.contains("gibt es schon"),
        "error message must render; got {html}"
    );
    assert!(
        html.contains(r#"value="foo""#),
        "user-typed name must round-trip into the name input on re-render; got {html}"
    );
}

#[tokio::test]
async fn update_invalid_template_renders_form_with_error() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "Hey {mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    let app = build_router(state.clone());
    // Control-char templates are rejected by `PingManager::edit_template`.
    // %01 is SOH, which `validate_template` rejects so the form re-renders.
    let body = format!(
        "_csrf={csrf}&template=draft%01control",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings/team")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let html = body_string(res).await;
    // The user's draft must round-trip — the literal control char is escaped
    // by askama, but the surrounding text must survive.
    assert!(
        html.contains("draft") && html.contains("control"),
        "user draft must round-trip into the form; got {html}"
    );
    // Error banner from `validate_template`.
    assert!(
        html.contains("class=\"error\""),
        "error banner must render; got {html}"
    );
    // Original template must NOT have been overwritten.
    let view = state.ping_actor.get_one("team".into()).await.unwrap();
    assert_eq!(view.template, "Hey {mentions}");
}

#[tokio::test]
async fn edit_form_lists_members() {
    let (state, sid, csrf, _bare, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    state
        .ping_actor
        .add_member("team".into(), "alice".into())
        .await
        .unwrap();
    state
        .ping_actor
        .add_member("team".into(), "bob".into())
        .await
        .unwrap();
    let app = build_router(state);
    let req = Request::builder()
        .uri("/pings/team")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert!(html.contains("alice"), "alice listed");
    assert!(html.contains("bob"), "bob listed");
    assert!(
        html.contains(r#"name="username""#),
        "add-member input present"
    );
}

#[tokio::test]
async fn add_member_round_trip() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    let body = format!(
        "_csrf={csrf}&username=Alice",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings/team/members")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let app = build_router(state.clone());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        state
            .ping_actor
            .is_member("team".into(), "alice".into())
            .await,
        "alice lowercased + added"
    );
}

#[tokio::test]
async fn add_member_rejects_invalid_login() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    let body = format!(
        "_csrf={csrf}&username=bad+name%21",
        csrf = urlencoding::encode(&bare_csrf),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings/team/members")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let app = build_router(state.clone());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let view = state.ping_actor.get_one("team".into()).await.unwrap();
    assert!(view.members.is_empty(), "invalid username must not persist");
}

#[tokio::test]
async fn remove_member_via_htmx_returns_oob_member_count() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    for m in ["alice", "bob"] {
        state
            .ping_actor
            .add_member("team".into(), m.into())
            .await
            .unwrap();
    }
    let req = Request::builder()
        .method("POST")
        .uri("/pings/team/members/alice/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header("X-Csrf-Token", bare_csrf.clone())
        .header("HX-Request", "true")
        .body(Body::empty())
        .unwrap();
    let app = build_router(state.clone());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;
    assert_eq!(
        html.trim(),
        r#"<span class="count" id="ping-member-count" hx-swap-oob="true">(1)</span>"#,
        "empty main swap removes the row; the heading count updates out of band"
    );
    assert!(
        !state
            .ping_actor
            .is_member("team".into(), "alice".into())
            .await,
        "alice must be removed"
    );
}

#[tokio::test]
async fn remove_member_without_htmx_redirects_to_edit_page() {
    let (state, sid, csrf, bare_csrf, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    state
        .ping_actor
        .add_member("team".into(), "alice".into())
        .await
        .unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/pings/team/members/alice/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header("X-Csrf-Token", bare_csrf.clone())
        .body(Body::empty())
        .unwrap();
    let app = build_router(state.clone());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/pings/team");
    assert!(
        !state
            .ping_actor
            .is_member("team".into(), "alice".into())
            .await,
        "alice must be removed"
    );
}

#[tokio::test]
async fn remove_member_without_csrf_rejected() {
    let (state, sid, csrf, _bare, _td) = authed_setup().await;
    state
        .ping_actor
        .create_ping("team".into(), "{mentions}".into(), "admin".into(), None)
        .await
        .unwrap();
    state
        .ping_actor
        .add_member("team".into(), "alice".into())
        .await
        .unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/pings/team/members/alice/delete")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let app = build_router(state.clone());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(
        state
            .ping_actor
            .is_member("team".into(), "alice".into())
            .await,
        "must persist on rejection"
    );
}

#[tokio::test]
async fn create_rejects_bad_form_csrf() {
    let (state, sid, csrf, _bare_csrf, _td) = authed_setup().await;
    let app = build_router(state.clone());
    let body = format!(
        "_csrf={bad}&name=tampered&template=hi",
        bad = urlencoding::encode(&"00".repeat(32)),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(
        state.ping_actor.get_one("tampered".into()).await.is_none(),
        "ping must not persist with mismatched _csrf",
    );
}

/// Build a minimal router that mounts the pings viewer routes under
/// `require_role(Viewer)` so tests can exercise viewer-rendered HTML via the
/// real viewer sub-router surface.
fn viewer_pings_app(state: twitch_1337_web::WebState) -> axum::Router {
    use axum::Router;
    use axum::middleware::from_fn_with_state;
    use tower_cookies::CookieManagerLayer;
    use twitch_1337_web::auth::{Role, require_role};
    use twitch_1337_web::routes::pings::viewer_router;

    Router::new()
        .merge(viewer_router())
        .route_layer(from_fn_with_state(state.clone(), move |s, c, r, n| {
            require_role(Role::Viewer, s, c, r, n)
        }))
        .with_state(state)
        .layer(CookieManagerLayer::new())
}

#[tokio::test]
async fn viewer_pings_list_has_no_mutation_controls() {
    install_crypto();
    let helix = Arc::new(FakeHelix {
        moderators: vec![],
        users: Default::default(),
    });
    let (state, _td) = build_state_with_ping_dir(helix).await;

    // Seed a ping so the table renders rows, not the empty state.
    state
        .ping_actor
        .create_ping("alpha".into(), "{mentions}".into(), "creator".into(), None)
        .await
        .unwrap();

    let (sid, csrf, _bare_csrf) =
        insert_session_as(&state, "42", "alice", twitch_1337_web::auth::Role::Viewer);

    let app = viewer_pings_app(state);
    let req = Request::builder()
        .uri("/pings")
        .method("GET")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    assert!(html.contains("alpha"), "ping name should render for viewer");
    assert!(
        !html.contains(r#"href="/pings/new""#),
        "viewer must not see +New button; got {html}"
    );
    assert!(
        !html.contains("hx-post=\"/pings/"),
        "viewer must not see delete buttons; got {html}"
    );
    assert!(
        html.contains(r#"href="/pings?sort=members""#),
        "sorting is read-only, so viewers get the sort chips too; got {html}"
    );
}

#[tokio::test]
async fn mod_pings_list_shows_mutation_controls() {
    install_crypto();
    let user_id = "9002";
    let helix = Arc::new(FakeHelix {
        moderators: vec![user_id.into()],
        users: Default::default(),
    });
    let (state, _td) = build_state_with_ping_dir(helix).await;

    state
        .ping_actor
        .create_ping("beta".into(), "{mentions}".into(), "creator".into(), None)
        .await
        .unwrap();

    let (sid, csrf, _bare_csrf) =
        insert_session_as(&state, user_id, "moduser", twitch_1337_web::auth::Role::Mod);

    let app = build_router(state);
    let req = Request::builder()
        .uri("/pings")
        .header(header::COOKIE, cookie_header(&sid, &csrf))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_string(res).await;

    assert!(html.contains("beta"), "ping name should render for mod");
    assert!(
        html.contains(r#"href="/pings/new""#),
        "mod must see +New button; got {html}"
    );
    assert!(
        html.contains("hx-post=\"/pings/"),
        "mod must see delete buttons; got {html}"
    );
    assert!(
        html.contains(r#"href="/pings?sort=members""#),
        "mod must see sort chips; got {html}"
    );
}
