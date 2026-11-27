//! Hub state machine against a wiremock GitHub.

use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::github::test_support::{bootstrap, mount_auth};
use crate::settings::Settings;

const REPO: &str = "o/r";

fn url(n: u64) -> String {
    format!("https://github.com/o/r/issues/{n}")
}

fn issue_json(n: u64) -> Value {
    json!({ "number": n, "html_url": url(n), "state": "open" })
}

fn settings() -> SettingsHandle {
    SettingsHandle::from_settings(Settings::compiled_defaults())
}

async fn github(server: &MockServer) -> Arc<GithubClient> {
    crate::install_crypto_provider();
    mount_auth(server, REPO).await;
    Arc::new(
        GithubClient::new(&bootstrap(REPO))
            .expect("client")
            .with_api_base(server.uri().parse().expect("mock uri")),
    )
}

struct Fixture {
    dir: TempDir,
    hub: Arc<FeedbackHub>,
    rx: mpsc::Receiver<HubCommand>,
}

async fn fixture(server: Option<&MockServer>) -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let gh = match server {
        Some(s) => Some(github(s).await),
        None => None,
    };
    let hub = FeedbackHub::open(dir.path(), gh, settings())
        .await
        .expect("open");
    let rx = hub.take_commands().expect("receiver");
    Fixture { dir, hub, rx }
}

fn form() -> PromoteForm {
    PromoteForm {
        title: "  !fl kaputt  ".into(),
        issue_type: IssueType::Bug,
        systems: vec!["aviation".into(), "aviation".into()],
        note: "Repro: `!fl EDDF`".into(),
    }
}

fn state(hub: &FeedbackHub, id: EntryId) -> EntryState {
    hub.entry(id).expect("entry").state
}

fn notices(rx: &mut mpsc::Receiver<HubCommand>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let HubCommand::Notice(line) = cmd {
            out.push(line);
        }
    }
    out
}

async fn requests(server: &MockServer, verb: &str, url_path: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .expect("recording")
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == url_path)
        .collect()
}

async fn mount_create(server: &MockServer, status: u16, times: u64) {
    let template = if status == 201 {
        ResponseTemplate::new(201).set_body_json(issue_json(7))
    } else {
        ResponseTemplate::new(status).set_body_json(json!({ "message": "nope" }))
    };
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues"))
        .respond_with(template)
        .up_to_n_times(times)
        .mount(server)
        .await;
}

/// Search hit `issue` for entry `id`: the item's body and one comment on the
/// issue carry the entry's exact hidden marker, as GitHub would return them.
async fn mount_search(server: &MockServer, hit: Option<(u64, EntryId)>) {
    let hidden = |id| format!("<!-- {} -->", crate::feedback::marker(id));
    let items: Vec<Value> = hit
        .into_iter()
        .map(|(n, id)| {
            let mut item = issue_json(n);
            item["body"] = json!(hidden(id));
            item
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "total_count": items.len(), "items": items })),
        )
        .mount(server)
        .await;
    if let Some((n, id)) = hit {
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/r/issues/{n}/comments")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{ "body": hidden(id) }])))
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn promote_creates_one_issue_links_and_notices() {
    let server = MockServer::start().await;
    mount_create(&server, 201, 1).await;
    let mut f = fixture(Some(&server)).await;
    let id = f
        .hub
        .submit("u1", "alice", "flugzeug weg")
        .await
        .expect("submit");
    let now = Timestamp::now();

    let link = f
        .hub
        .promote_at(id, form(), "modbert", now)
        .await
        .expect("promote");
    assert_eq!(
        link,
        IssueLink {
            number: 7,
            url: url(7)
        }
    );
    assert!(matches!(
        &state(&f.hub, id),
        EntryState::Linked { issue: 7, via: LinkVia::Promoted, by, close: None, .. } if by == "modbert"
    ));
    assert_eq!(
        notices(&mut f.rx),
        vec![format!(
            "@alice dein Feedback F{id} ist jetzt ein Issue: {}",
            url(7)
        )]
    );

    let created = requests(&server, "POST", "/repos/o/r/issues").await;
    assert_eq!(created.len(), 1);
    let body: Value = serde_json::from_slice(&created[0].body).expect("json");
    assert_eq!(body["title"], "!fl kaputt");
    assert_eq!(body["type"], "Bug");
    assert_eq!(
        body["labels"],
        json!(["status:needs-triage", "source:chat", "system:aviation"])
    );
    let text = body["body"].as_str().expect("body");
    assert!(text.starts_with("Repro: `!fl EDDF`\n\n---"), "{text}");
    assert!(text.contains(&format!("<!-- twitch-1337-feedback:F{id} -->")));

    // Already linked: a second click creates nothing.
    assert!(matches!(
        f.hub.promote_at(id, form(), "modbert", now).await,
        Err(ActionError::InvalidState)
    ));
    assert_eq!(
        requests(&server, "POST", "/repos/o/r/issues").await.len(),
        1
    );
}

#[tokio::test]
async fn promote_while_in_flight_is_busy() {
    let server = MockServer::start().await;
    mount_create(&server, 201, 1).await;
    let f = fixture(Some(&server)).await;
    let id = f.hub.submit("u1", "alice", "x").await.expect("submit");
    let now = Timestamp::now();
    f.hub
        .set_state(
            id,
            EntryState::Promoting {
                since: now,
                by: "other".into(),
            },
        )
        .await;
    assert!(matches!(
        f.hub.promote_at(id, form(), "modbert", now).await,
        Err(ActionError::Busy)
    ));
    assert!(matches!(
        f.hub.dismiss(id, "modbert").await,
        Err(ActionError::Busy)
    ));
    assert!(
        requests(&server, "POST", "/repos/o/r/issues")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn ambiguous_promote_holds_off_then_creates_once() {
    let server = MockServer::start().await;
    mount_create(&server, 502, 1).await;
    mount_create(&server, 201, 1).await;
    mount_search(&server, None).await;
    let f = fixture(Some(&server)).await;
    let id = f.hub.submit("u1", "alice", "x").await.expect("submit");
    let t0 = Timestamp::now();

    assert!(matches!(
        f.hub.promote_at(id, form(), "modbert", t0).await,
        Err(ActionError::Ambiguous)
    ));
    let parked = EntryState::PromoteUnknown {
        failed_at: t0,
        by: "modbert".into(),
    };
    assert_eq!(state(&f.hub, id), parked);

    // Retry within 2 min: marker not indexed yet, so nothing is created.
    let t1 = t0 + SignedDuration::from_secs(30);
    assert!(matches!(
        f.hub.promote_at(id, form(), "modbert", t1).await,
        Err(ActionError::StillChecking)
    ));
    assert_eq!(state(&f.hub, id), parked);
    assert_eq!(
        requests(&server, "POST", "/repos/o/r/issues").await.len(),
        1
    );

    // After the hold-off the search still finds nothing: create again.
    let t2 = t0 + SignedDuration::from_mins(3);
    let link = f
        .hub
        .promote_at(id, form(), "modbert", t2)
        .await
        .expect("promote");
    assert_eq!(link.number, 7);
    assert_eq!(
        requests(&server, "POST", "/repos/o/r/issues").await.len(),
        2
    );
    let searches = requests(&server, "GET", "/search/issues").await;
    assert_eq!(searches.len(), 2);
    let q = searches[0]
        .url
        .query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.into_owned())
        .expect("q");
    assert!(
        q.contains(&format!("\"twitch-1337-feedback:F{id}\"")),
        "{q}"
    );
    assert!(q.contains("in:body"), "{q}");
}

#[tokio::test]
async fn ambiguous_promote_resolved_by_marker_creates_nothing() {
    let server = MockServer::start().await;
    mount_create(&server, 504, 1).await;
    mount_search(&server, Some((7, 1))).await;
    let mut f = fixture(Some(&server)).await;
    let id = f.hub.submit("u1", "alice", "x").await.expect("submit");
    let t0 = Timestamp::now();
    assert!(matches!(
        f.hub.promote_at(id, form(), "modbert", t0).await,
        Err(ActionError::Ambiguous)
    ));
    let link = f
        .hub
        .promote_at(id, form(), "modbert", t0 + SignedDuration::from_secs(5))
        .await
        .expect("resolved");
    assert_eq!(
        link,
        IssueLink {
            number: 7,
            url: url(7)
        }
    );
    assert!(matches!(
        state(&f.hub, id),
        EntryState::Linked {
            issue: 7,
            via: LinkVia::Promoted,
            ..
        }
    ));
    assert_eq!(
        requests(&server, "POST", "/repos/o/r/issues").await.len(),
        1
    );
    assert_eq!(notices(&mut f.rx).len(), 1);
}

#[tokio::test]
async fn rejected_promote_returns_to_new() {
    let server = MockServer::start().await;
    mount_create(&server, 422, 1).await;
    let mut f = fixture(Some(&server)).await;
    let id = f.hub.submit("u1", "alice", "x").await.expect("submit");
    assert!(matches!(
        f.hub
            .promote_at(id, form(), "modbert", Timestamp::now())
            .await,
        Err(ActionError::Github(GithubError::Rejected {
            status: 422,
            ..
        }))
    ));
    assert_eq!(state(&f.hub, id), EntryState::New);
    assert!(notices(&mut f.rx).is_empty());
}

#[tokio::test]
async fn promote_validates_before_anything_else() {
    let f = fixture(None).await;
    let id = f.hub.submit("u1", "alice", "x").await.expect("submit");
    let mut bad = form();
    bad.title = " \u{7} ".into();
    assert!(matches!(
        f.hub.promote(id, bad, "m").await,
        Err(ActionError::Invalid {
            field: "title",
            reason: InvalidReason::Empty
        })
    ));
    let mut bad = form();
    bad.systems = vec!["kitchen".into()];
    assert!(matches!(
        f.hub.promote(id, bad, "m").await,
        Err(ActionError::Invalid {
            field: "systems",
            reason: InvalidReason::UnknownSystem
        })
    ));
    assert!(matches!(
        f.hub.promote(id, form(), "m").await,
        Err(ActionError::GithubUnconfigured)
    ));
    assert!(matches!(
        f.hub.attach(id, 0, "m").await,
        Err(ActionError::Invalid {
            field: "issue",
            reason: InvalidReason::Zero
        })
    ));
    assert!(matches!(
        f.hub.link_manual(id, 3, "m").await,
        Err(ActionError::GithubUnconfigured)
    ));
    assert_eq!(state(&f.hub, id), EntryState::New);
}

#[tokio::test]
async fn attach_comments_labels_links_and_notices() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/12"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(12)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/12/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/12/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = fixture(Some(&server)).await;
    let id = f.hub.submit("u2", "bob", "same").await.expect("submit");
    let link = f.hub.attach(id, 12, "modbert").await.expect("attach");
    assert_eq!(
        link,
        IssueLink {
            number: 12,
            url: url(12)
        }
    );
    assert!(matches!(
        state(&f.hub, id),
        EntryState::Linked {
            issue: 12,
            via: LinkVia::Attached,
            ..
        }
    ));
    assert_eq!(
        notices(&mut f.rx),
        vec![format!(
            "@bob dein Feedback F{id} hängt jetzt an #12: {}",
            url(12)
        )]
    );
    let comment = &requests(&server, "POST", "/repos/o/r/issues/12/comments").await[0];
    let body: Value = serde_json::from_slice(&comment.body).expect("json");
    let text = body["body"].as_str().expect("body");
    assert!(text.starts_with("+1 from Twitch chat: `bob`"), "{text}");
    let labels = &requests(&server, "POST", "/repos/o/r/issues/12/labels").await[0];
    let body: Value = serde_json::from_slice(&labels.body).expect("json");
    assert_eq!(body["labels"], json!(["source:chat"]));
}

#[tokio::test]
async fn attach_to_missing_issue_returns_to_the_previous_state() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/99"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/12"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(12)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/12/comments"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let f = fixture(Some(&server)).await;
    let id = f.hub.submit("u2", "bob", "same").await.expect("submit");
    assert!(matches!(
        f.hub.attach(id, 99, "m").await,
        Err(ActionError::IssueNotFound)
    ));
    assert_eq!(state(&f.hub, id), EntryState::New);
    assert!(
        requests(&server, "POST", "/repos/o/r/issues/99/comments")
            .await
            .is_empty()
    );

    // The comment on #12 may have landed. A mistyped #99 must not forget
    // that, or the next attach to #12 would comment again without a check.
    let t0 = Timestamp::now();
    assert!(matches!(
        f.hub.attach_at(id, 12, "m", t0).await,
        Err(ActionError::Ambiguous)
    ));
    assert!(matches!(
        f.hub.attach_at(id, 99, "m", t0).await,
        Err(ActionError::IssueNotFound)
    ));
    assert!(matches!(
        state(&f.hub, id),
        EntryState::AttachUnknown { issue: 12, .. }
    ));
}

#[tokio::test]
async fn ambiguous_attach_parks_then_marker_in_comments_resolves() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/12"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(12)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/12/comments"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/12/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    mount_search(&server, Some((12, 1))).await;
    let f = fixture(Some(&server)).await;
    let id = f.hub.submit("u2", "bob", "same").await.expect("submit");
    let t0 = Timestamp::now();
    assert!(matches!(
        f.hub.attach_at(id, 12, "m", t0).await,
        Err(ActionError::Ambiguous)
    ));
    assert_eq!(
        state(&f.hub, id),
        EntryState::AttachUnknown {
            issue: 12,
            failed_at: t0,
            by: "m".into()
        }
    );
    // Promote is not allowed from an ambiguous attach.
    assert!(matches!(
        f.hub.promote_at(id, form(), "m", t0).await,
        Err(ActionError::InvalidState)
    ));
    let link = f
        .hub
        .attach_at(id, 12, "m", t0 + SignedDuration::from_secs(10))
        .await
        .expect("resolved");
    assert_eq!(link.number, 12);
    let q = requests(&server, "GET", "/search/issues").await[0]
        .url
        .query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.into_owned())
        .expect("q");
    assert!(q.contains("in:comments"), "{q}");
}

#[tokio::test]
async fn link_manual_checks_issue_without_writing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/12"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(12)))
        .mount(&server)
        .await;
    let f = fixture(Some(&server)).await;
    let id = f.hub.submit("u2", "bob", "same").await.expect("submit");
    f.hub
        .set_state(
            id,
            EntryState::PromoteUnknown {
                failed_at: Timestamp::now(),
                by: "m".into(),
            },
        )
        .await;
    let link = f.hub.link_manual(id, 12, "m2").await.expect("link");
    assert_eq!(link.number, 12);
    assert!(matches!(
        state(&f.hub, id),
        EntryState::Linked {
            issue: 12,
            via: LinkVia::Manual,
            ..
        }
    ));
    let posts = server
        .received_requests()
        .await
        .expect("recording")
        .into_iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().starts_with("/repos/"))
        .count();
    assert_eq!(posts, 0);
}

#[tokio::test]
async fn dismiss_restore_and_persisted_round_trip() {
    let f = fixture(None).await;
    let id = f.hub.submit("u1", "alice", "x").await.expect("submit");
    f.hub.dismiss(id, "m").await.expect("dismiss");
    assert!(matches!(
        &state(&f.hub, id),
        EntryState::Dismissed { by: Some(b), .. } if b == "m"
    ));
    assert!(matches!(
        f.hub.dismiss(id, "m").await,
        Err(ActionError::InvalidState)
    ));
    f.hub.restore(id).await.expect("restore");
    assert_eq!(state(&f.hub, id), EntryState::New);
    assert!(matches!(
        f.hub.restore(id).await,
        Err(ActionError::InvalidState)
    ));
    assert!(matches!(
        f.hub.dismiss(999, "m").await,
        Err(ActionError::NotFound)
    ));

    // An entry whose promote outcome is unknown cannot be dismissed: Restore
    // would bring it back as New and skip the marker check.
    let mut unknown = f.hub.entry(id).expect("entry");
    unknown.id = 50;
    unknown.state = EntryState::PromoteUnknown {
        failed_at: Timestamp::now(),
        by: "m".into(),
    };
    f.hub.insert_for_tests(unknown).await;
    assert!(matches!(
        f.hub.dismiss(50, "m").await,
        Err(ActionError::InvalidState)
    ));

    let reopened = FeedbackHub::open(f.dir.path(), None, settings())
        .await
        .expect("reopen");
    assert_eq!(reopened.entries(), f.hub.entries());
}

#[tokio::test]
async fn startup_recovers_in_flight_states_and_prunes() {
    let f = fixture(None).await;
    let now = Timestamp::now();
    let base = FeedbackEntry {
        id: 1,
        user_id: "u".into(),
        login: "a".into(),
        text: "x".into(),
        created_at: now,
        classification: None,
        state: EntryState::Promoting {
            since: now,
            by: "m".into(),
        },
    };
    f.hub.insert_for_tests(base.clone()).await;
    f.hub
        .insert_for_tests(FeedbackEntry {
            id: 2,
            state: EntryState::Attaching {
                issue: 4,
                since: now,
                by: "m".into(),
            },
            ..base.clone()
        })
        .await;
    f.hub
        .insert_for_tests(FeedbackEntry {
            id: 3,
            state: EntryState::Dismissed {
                at: now - SignedDuration::from_hours(31 * 24),
                by: None,
            },
            ..base
        })
        .await;

    let reopened = FeedbackHub::open(f.dir.path(), None, settings())
        .await
        .expect("reopen");
    assert_eq!(
        state(&reopened, 1),
        EntryState::PromoteUnknown {
            failed_at: now,
            by: "m".into()
        }
    );
    assert_eq!(
        state(&reopened, 2),
        EntryState::AttachUnknown {
            issue: 4,
            failed_at: now,
            by: "m".into()
        }
    );
    assert!(reopened.entry(3).is_none());
    // The recovery was persisted, and ids keep counting past the seeds.
    let again = FeedbackHub::open(f.dir.path(), None, settings())
        .await
        .expect("again");
    assert_eq!(again.entries(), reopened.entries());
    assert_eq!(again.submit("u9", "z", "y").await.expect("submit"), 4);
}

#[tokio::test]
async fn submit_caps_per_user_and_queues_classify_when_profile_set() {
    let dir = TempDir::new().expect("tempdir");
    let mut s = Settings::compiled_defaults();
    s.feedback.per_user_daily = 2;
    s.feedback.profile = "dec".into();
    let hub = FeedbackHub::open(dir.path(), None, SettingsHandle::from_settings(s))
        .await
        .expect("open");
    let mut rx = hub.take_commands().expect("rx");
    assert!(hub.take_commands().is_none());
    let a = hub.submit("u1", "alice", "one").await.expect("1");
    hub.submit("u1", "alice", "two").await.expect("2");
    assert!(matches!(
        hub.submit("u1", "alice", "three").await,
        Err(SubmitError::DailyCap)
    ));
    assert_eq!(rx.try_recv().ok(), Some(HubCommand::Classify(a)));
    assert_eq!(hub.entries().len(), 2);
    assert_eq!(hub.entries()[0].text, "two", "newest first");
    hub.request_classify(a).expect("classify");
    assert!(matches!(
        hub.request_classify(77),
        Err(ActionError::NotFound)
    ));
}

#[tokio::test]
async fn close_poll_sets_cursor_then_announces_completed_once() {
    let server = MockServer::start().await;
    let f = fixture(Some(&server)).await;
    let now = Timestamp::now();
    for (id, login) in [(1, "alice"), (2, "bob"), (3, "carol")] {
        f.hub
            .insert_for_tests(FeedbackEntry {
                id,
                user_id: format!("u{id}"),
                login: login.into(),
                text: "x".into(),
                created_at: now,
                classification: None,
                state: EntryState::Linked {
                    issue: if id == 3 { 8 } else { 5 },
                    via: LinkVia::Promoted,
                    by: "m".into(),
                    at: now,
                    close: None,
                },
            })
            .await;
    }

    // First poll: no cursor yet, so no request; the cursor becomes now.
    assert!(f.hub.poll_closes(now).await.expect("first").is_empty());
    assert!(
        requests(&server, "GET", "/repos/o/r/issues")
            .await
            .is_empty()
    );
    assert_eq!(f.hub.state.lock().inbox.cursor, Some(now));

    let closed_at = |t: Timestamp| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .and(query_param("state", "closed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "number": 5, "html_url": url(5), "state": "closed",
                "state_reason": "completed",
                "closed_at": closed_at(now), "updated_at": closed_at(now),
            },
            {
                "number": 8, "html_url": url(8), "state": "closed",
                "state_reason": "not_planned",
                "closed_at": closed_at(now), "updated_at": closed_at(now),
            },
        ])))
        .mount(&server)
        .await;
    let later = now + SignedDuration::from_mins(10);
    let lines = f.hub.poll_closes(later).await.expect("poll");
    assert_eq!(
        lines,
        vec![format!(
            "@alice @bob euer Feedback zu #5 ist erledigt: {}",
            url(5)
        )]
    );
    assert!(matches!(
        state(&f.hub, 3),
        EntryState::Linked {
            close: Some(CloseInfo {
                completed: false,
                notified: false,
                ..
            }),
            ..
        }
    ));
    // The cursor is inclusive, so the same closes come back: still silent.
    assert!(f.hub.poll_closes(later).await.expect("again").is_empty());
}
