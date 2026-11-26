//! Integration tests for the mod-gated `/feedback` inbox. The test state's
//! hub has no GitHub client, so Promote answers `GithubUnconfigured`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use jiff::Timestamp;
use tempfile::TempDir;
use tower::ServiceExt as _;
use twitch_1337_core::feedback::{EntryState, FeedbackEntry};
use twitch_1337_web::auth::Role;
use twitch_1337_web::{WebState, build_router};

use super::helpers;
use helpers::{
    FakeHelix, build_state_with_dirs, cookie_header, insert_session_as, install_crypto, set_owner,
};

const OWNER_ID: &str = "999";

struct Fixture {
    state: WebState,
    sid: String,
    csrf_cookie: String,
    csrf: String,
    // Hosts `feedback.ron`; dropping it would fail the hub's saves.
    _td_pings: TempDir,
    _td_mem: TempDir,
}

/// State with one New entry `F1` and a session for `role`.
async fn fixture(user_id: &str, role: Role) -> Fixture {
    install_crypto();
    let helix = Arc::new(FakeHelix {
        users: HashMap::new(),
    });
    let (state, td_pings, td_mem) = build_state_with_dirs(helix).await;
    set_owner(&state, Some(OWNER_ID));
    state
        .feedback
        .insert_for_tests(new_entry(1, "<b>bold</b> & the ping command is broken"))
        .await;
    let (sid, csrf_cookie, csrf) = insert_session_as(&state, user_id, "someone", role);
    Fixture {
        state,
        sid,
        csrf_cookie,
        csrf,
        _td_pings: td_pings,
        _td_mem: td_mem,
    }
}

fn new_entry(id: u64, text: &str) -> FeedbackEntry {
    FeedbackEntry {
        id,
        user_id: format!("u{id}"),
        login: format!("viewer{id}"),
        text: text.to_owned(),
        created_at: Timestamp::now(),
        classification: None,
        state: EntryState::New,
    }
}

impl Fixture {
    async fn get(&self, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie_header(&self.sid, &self.csrf_cookie))
            .body(Body::empty())
            .unwrap();
        send(&self.state, req).await
    }

    async fn post(&self, uri: &str, body: String, htmx: bool) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::COOKIE, cookie_header(&self.sid, &self.csrf_cookie))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if htmx {
            req = req.header("HX-Request", "true");
        }
        send(&self.state, req.body(Body::from(body)).unwrap()).await
    }

    fn csrf_field(&self) -> String {
        format!("_csrf={}", urlencoding::encode(&self.csrf))
    }

    fn state_of(&self, id: u64) -> EntryState {
        self.state.feedback.entry(id).expect("entry exists").state
    }
}

async fn send(state: &WebState, req: Request<Body>) -> (StatusCode, String) {
    let res = build_router(state.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn mod_sees_entry_with_escaped_text_and_nav_link() {
    let f = fixture("42", Role::Mod).await;
    let (status, body) = f.get("/feedback").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("id=\"feedback-1\""), "F1 card rendered");
    assert!(body.contains("F1"));
    assert!(body.contains("viewer1"), "login shown");
    // askama escapes `<`, `>`, `&` as `&#60;`, `&#62;`, `&#38;`.
    assert!(
        body.contains("&#60;b&#62;bold&#60;/b&#62; &#38; the ping command is broken"),
        "viewer text is escaped: {body}"
    );
    assert!(
        !body.contains("<b>bold</b>"),
        "viewer text must not render raw"
    );
    assert!(
        body.contains("href=\"/feedback\""),
        "sidebar offers Feedback"
    );
    assert!(body.contains("Not classified"));
    assert!(
        body.contains(
            "name=\"title\" value=\"&#60;b&#62;bold&#60;/b&#62; &#38; the ping command is broken\""
        ),
        "title prefilled from the text"
    );
    assert!(
        body.contains("GitHub is not configured"),
        "promote/attach disabled without GitHub"
    );
}

#[tokio::test]
async fn owner_can_open_the_inbox() {
    let f = fixture(OWNER_ID, Role::Owner).await;
    let (status, body) = f.get("/feedback").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("id=\"feedback-1\""));
}

#[tokio::test]
async fn allowlisted_viewer_is_refused_and_sees_no_nav_link() {
    let f = fixture("42", Role::Viewer).await;
    let (status, _) = f.get("/feedback").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = f.get("/leaderboard").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("href=\"/feedback\""),
        "Feedback nav entry hidden for viewers"
    );
}

#[tokio::test]
async fn dismiss_moves_entry_to_dismissed_and_restore_brings_it_back() {
    let f = fixture("42", Role::Mod).await;

    let (status, _) = f.post("/feedback/1/dismiss", f.csrf_field(), false).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(matches!(
        f.state_of(1),
        EntryState::Dismissed { by: Some(_), .. }
    ));

    let (_, new_tab) = f.get("/feedback?tab=new").await;
    assert!(!new_tab.contains("id=\"feedback-1\""), "gone from New");
    let (_, dismissed) = f.get("/feedback?tab=dismissed").await;
    assert!(
        dismissed.contains("id=\"feedback-1\""),
        "listed in Dismissed"
    );
    assert!(dismissed.contains("Restore"));
    assert!(
        dismissed.contains("Deleted in"),
        "shows days until deletion"
    );

    let (status, _) = f.post("/feedback/1/restore", f.csrf_field(), false).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(matches!(f.state_of(1), EntryState::New));
    let (_, new_tab) = f.get("/feedback?tab=new").await;
    assert!(new_tab.contains("id=\"feedback-1\""), "back in New");
}

#[tokio::test]
async fn htmx_dismiss_swaps_the_card_and_tab_counts() {
    let f = fixture("42", Role::Mod).await;
    let (status, body) = f.post("/feedback/1/dismiss", f.csrf_field(), true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("id=\"feedback-1\""), "card swapped in");
    assert!(body.contains("Restore"), "dismissed card offers Restore");
    assert!(body.contains("F1 dismissed."), "notice on the card");
    assert!(
        body.contains("id=\"feedback-tabs\" aria-label=\"Feedback tabs\" hx-swap-oob=\"true\"")
    );
    assert!(
        body.contains("id=\"feedback-count-new\">0<"),
        "New count updated"
    );
}

#[tokio::test]
async fn promote_without_github_reports_it_and_keeps_entry_new() {
    let f = fixture("42", Role::Mod).await;
    let form = format!(
        "{}&title=Ping+broken&issue_type=Bug&systems=ping&note=",
        f.csrf_field()
    );

    let (status, body) = f.post("/feedback/1/promote", form.clone(), false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("GitHub is not configured."), "{body}");
    assert!(matches!(f.state_of(1), EntryState::New));

    let (status, body) = f.post("/feedback/1/promote", form, true).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "GitHub is not configured.", "htmx gets toast text");
    assert!(matches!(f.state_of(1), EntryState::New));
}

#[tokio::test]
async fn promote_with_empty_title_marks_the_title_field() {
    let f = fixture("42", Role::Mod).await;
    let form = format!("{}&title=+++&issue_type=Task&note=keep+me", f.csrf_field());

    let (status, body) = f.post("/feedback/1/promote", form.clone(), false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("aria-invalid=\"true\" aria-describedby=\"fb-1-title-error\""),
        "title control flagged: {body}"
    );
    assert!(body.contains("id=\"fb-1-title-error\">Title must not be empty.</p>"));
    assert!(body.contains(">keep me</textarea>"), "note input kept");
    assert!(matches!(f.state_of(1), EntryState::New));

    // htmx swaps the card (200) so the error renders beside the control.
    let (status, body) = f.post("/feedback/1/promote", form, true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("aria-describedby=\"fb-1-title-error\""));
}

#[tokio::test]
async fn post_without_csrf_is_rejected() {
    let f = fixture("42", Role::Mod).await;

    let (status, _) = f.post("/feedback/1/dismiss", String::new(), false).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(matches!(f.state_of(1), EntryState::New));

    let (status, _) = f
        .post(
            "/feedback/1/promote",
            "_csrf=00&title=x&issue_type=Bug".to_owned(),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(matches!(f.state_of(1), EntryState::New));
}
