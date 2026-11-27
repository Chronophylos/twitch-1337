use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::test_support::{TEST_PRIVATE_KEY_PEM, TEST_TOKEN, bootstrap, mount_auth};
use super::*;

const REPO: &str = "o/r";

fn client(server: &MockServer) -> GithubClient {
    crate::install_crypto_provider();
    GithubClient::new(&bootstrap(REPO))
        .expect("client")
        .with_api_base(server.uri().parse().expect("mock uri"))
}

async fn setup() -> (MockServer, GithubClient) {
    let server = MockServer::start().await;
    mount_auth(&server, REPO).await;
    let client = client(&server);
    (server, client)
}

fn issue_json(number: u64, state: &str) -> Value {
    json!({
        "number": number,
        "html_url": format!("https://github.com/o/r/issues/{number}"),
        "state": state,
        "updated_at": "2026-11-26T12:00:00Z",
    })
}

async fn mount_issue(server: &MockServer, number: u64, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/o/r/issues/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn requests_to(server: &MockServer, verb: &str, url_path: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .expect("recording on")
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == url_path)
        .collect()
}

#[tokio::test]
async fn app_jwt_is_rs256_signed_by_the_configured_key() {
    let (server, client) = setup().await;
    mount_issue(&server, 5, issue_json(5, "open")).await;
    client.get_issue(5).await.expect("issue");

    let lookups = requests_to(&server, "GET", "/repos/o/r/installation").await;
    assert_eq!(lookups.len(), 1);
    let auth = lookups[0]
        .headers
        .get("authorization")
        .expect("auth header")
        .to_str()
        .expect("ascii");
    let jwt = auth.strip_prefix("Bearer ").expect("bearer");
    let mut parts = jwt.split('.');
    let (header_b64, claims_b64, sig_b64) = (
        parts.next().expect("header"),
        parts.next().expect("claims"),
        parts.next().expect("signature"),
    );
    assert!(parts.next().is_none());

    let key = parse_private_key(TEST_PRIVATE_KEY_PEM).expect("test key");
    let signature = BASE64URL.decode(sig_b64).expect("b64 signature");
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref())
        .verify(format!("{header_b64}.{claims_b64}").as_bytes(), &signature)
        .expect("signature verifies with the public half");

    let header: Value =
        serde_json::from_slice(&BASE64URL.decode(header_b64).expect("b64")).expect("json");
    assert_eq!(header, json!({ "alg": "RS256", "typ": "JWT" }));
    let claims: Value =
        serde_json::from_slice(&BASE64URL.decode(claims_b64).expect("b64")).expect("json");
    assert_eq!(claims["iss"], json!(1));
    let iat = claims["iat"].as_i64().expect("iat");
    let exp = claims["exp"].as_i64().expect("exp");
    assert_eq!(exp - iat, 600);
    let now = Timestamp::now().as_second();
    assert!((now - 120..=now - 60).contains(&iat), "iat {iat} now {now}");

    // The installation token, not the JWT, authorizes the API call.
    let calls = requests_to(&server, "GET", "/repos/o/r/issues/5").await;
    let api_auth = calls[0].headers.get("authorization").expect("auth");
    assert_eq!(
        api_auth.to_str().expect("ascii"),
        format!("Bearer {TEST_TOKEN}")
    );
}

#[tokio::test]
async fn installation_token_is_cached_across_calls() {
    let (server, client) = setup().await;
    mount_issue(&server, 5, issue_json(5, "open")).await;
    client.get_issue(5).await.expect("first");
    client.get_issue(5).await.expect("second");
    assert_eq!(
        requests_to(&server, "POST", "/app/installations/1/access_tokens")
            .await
            .len(),
        1
    );
    assert_eq!(
        requests_to(&server, "GET", "/repos/o/r/installation")
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn expiring_token_is_refreshed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 9 })))
        .expect(1)
        .mount(&server)
        .await;
    // Expires in 4 minutes: inside the 5 minute refresh margin.
    let expires_at = Timestamp::now() + SignedDuration::from_mins(4);
    Mock::given(method("POST"))
        .and(path("/app/installations/9/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_short",
            "expires_at": expires_at.to_string(),
        })))
        .expect(2)
        .mount(&server)
        .await;
    mount_issue(&server, 5, issue_json(5, "open")).await;
    let client = client(&server);
    client.get_issue(5).await.expect("first");
    client.get_issue(5).await.expect("second");
}

#[tokio::test]
async fn create_issue_sends_type_and_labels() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues"))
        .and(header("authorization", "Bearer ghs_test"))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(body_partial_json(json!({
            "title": "Ping kaputt",
            "body": "text <!-- twitch-1337-feedback:F42 -->",
            "type": "Bug",
            "labels": ["status:needs-triage", "source:chat", "system:ping"],
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(issue_json(12, "open")))
        .expect(1)
        .mount(&server)
        .await;
    let issue = client
        .create_issue(&NewIssue {
            title: "Ping kaputt".to_owned(),
            body: "text <!-- twitch-1337-feedback:F42 -->".to_owned(),
            issue_type: IssueType::Bug,
            labels: vec![
                "status:needs-triage".to_owned(),
                "source:chat".to_owned(),
                "system:ping".to_owned(),
            ],
        })
        .await
        .expect("created");
    assert_eq!(
        issue,
        IssueRef {
            number: 12,
            html_url: "https://github.com/o/r/issues/12".to_owned(),
        }
    );
}

fn new_issue() -> NewIssue {
    NewIssue {
        title: "t".to_owned(),
        body: "b".to_owned(),
        issue_type: IssueType::Task,
        labels: Vec::new(),
    }
}

#[tokio::test]
async fn server_error_is_ambiguous() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
        .mount(&server)
        .await;
    let err = client.create_issue(&new_issue()).await.expect_err("502");
    assert!(
        matches!(&err, GithubError::Ambiguous(GithubCause::Status { status, .. }) if status.as_u16() == 502),
        "{err:?}"
    );
}

#[tokio::test]
async fn unparseable_success_is_ambiguous() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(201).set_body_string("not json"))
        .mount(&server)
        .await;
    let err = client
        .create_issue(&new_issue())
        .await
        .expect_err("bad body");
    assert!(
        matches!(err, GithubError::Ambiguous(GithubCause::Parse(_))),
        "{err:?}"
    );
}

#[tokio::test]
async fn validation_failure_is_rejected_with_message() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Validation Failed",
            "errors": [{ "resource": "Issue", "code": "invalid", "field": "type" }],
            "documentation_url": "https://docs.github.com/rest/issues/issues#create-an-issue",
        })))
        .mount(&server)
        .await;
    let err = client.create_issue(&new_issue()).await.expect_err("422");
    match err {
        GithubError::Rejected { status, message } => {
            assert_eq!(status, 422);
            assert_eq!(message, "Validation Failed: type invalid");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn forbidden_is_auth() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/3/comments"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({ "message": "API rate limit exceeded" })),
        )
        .mount(&server)
        .await;
    let err = client.comment(3, "hi").await.expect_err("403");
    assert!(
        matches!(&err, GithubError::Auth(GithubCause::Status { message, .. }) if message == "API rate limit exceeded"),
        "{err:?}"
    );
}

#[tokio::test]
async fn unauthorized_drops_the_cached_token() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/3/labels"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    client
        .add_labels(3, &["source:chat"])
        .await
        .expect_err("401");
    client
        .add_labels(3, &["source:chat"])
        .await
        .expect_err("401");
    assert_eq!(
        requests_to(&server, "POST", "/app/installations/1/access_tokens")
            .await
            .len(),
        2
    );
}

#[tokio::test]
async fn missing_installation_is_auth() {
    crate::install_crypto_provider();
    let server = MockServer::start().await;
    // No auth mocks: wiremock answers the installation lookup with 404.
    let err = client(&server).get_issue(1).await.expect_err("no install");
    assert!(
        matches!(&err, GithubError::Auth(GithubCause::Installation(inner)) if matches!(**inner, GithubError::NotFound)),
        "{err:?}"
    );
}

#[tokio::test]
async fn missing_issue_and_pull_request_are_not_found() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&server)
        .await;
    let mut pr = issue_json(8, "open");
    pr["pull_request"] = json!({ "url": "https://api.github.com/repos/o/r/pulls/8" });
    mount_issue(&server, 8, pr).await;
    mount_issue(&server, 9, issue_json(9, "closed")).await;

    assert!(matches!(
        client.get_issue(404).await,
        Err(GithubError::NotFound)
    ));
    assert!(matches!(
        client.get_issue(8).await,
        Err(GithubError::NotFound)
    ));
    client.get_issue(9).await.expect("closed issue");
}

#[tokio::test]
async fn comment_and_labels_send_their_bodies() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/3/comments"))
        .and(body_partial_json(json!({ "body": "+1" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/3/labels"))
        .and(body_partial_json(json!({ "labels": ["source:chat"] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{ "name": "source:chat" }])))
        .expect(1)
        .mount(&server)
        .await;
    client.comment(3, "+1").await.expect("comment");
    client
        .add_labels(3, &["source:chat"])
        .await
        .expect("labels");
}

#[tokio::test]
async fn closed_since_paginates_and_skips_pull_requests() {
    let (server, client) = setup().await;
    let closed = |number: u64, reason: Value, closed_at: &str| {
        json!({
            "number": number,
            "html_url": format!("https://github.com/o/r/issues/{number}"),
            "state": "closed",
            "state_reason": reason,
            "closed_at": closed_at,
            "updated_at": closed_at,
        })
    };
    let mut pr = closed(2, Value::Null, "2026-11-26T10:00:00Z");
    pr["pull_request"] = json!({ "url": "https://api.github.com/repos/o/r/pulls/2" });
    let page = |n: &str| {
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues"))
            .and(query_param("state", "closed"))
            .and(query_param("since", "2026-11-26T09:00:00Z"))
            .and(query_param("sort", "updated"))
            .and(query_param("direction", "asc"))
            .and(query_param("per_page", "100"))
            .and(query_param("page", n))
    };
    let next = format!(
        r#"<{}/repositories/1/issues?page=2>; rel="next", <{}/repositories/1/issues?page=2>; rel="last""#,
        server.uri(),
        server.uri()
    );
    page("1")
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", next.as_str())
                .set_body_json(json!([
                    closed(1, json!("completed"), "2026-11-26T09:30:00Z"),
                    pr,
                ])),
        )
        .expect(1)
        .mount(&server)
        .await;
    page("2")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            closed(3, json!("not_planned"), "2026-11-26T11:00:00Z"),
            closed(4, json!("duplicate"), "2026-11-26T12:00:00Z"),
            closed(5, Value::Null, "2026-11-26T13:00:00Z"),
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let since: Timestamp = "2026-11-26T09:00:00.5Z".parse().expect("ts");
    let issues = client.closed_since(since).await.expect("listing");
    let summary: Vec<(u64, CloseReason)> = issues.iter().map(|i| (i.number, i.reason)).collect();
    assert_eq!(
        summary,
        vec![
            (1, CloseReason::Completed),
            (3, CloseReason::NotPlanned),
            (4, CloseReason::Duplicate),
            (5, CloseReason::Other),
        ]
    );
    assert_eq!(
        issues[0].closed_at,
        "2026-11-26T09:30:00Z".parse::<Timestamp>().expect("ts")
    );
    assert_eq!(issues[0].html_url, "https://github.com/o/r/issues/1");
}

#[tokio::test]
async fn find_marker_only_accepts_the_exact_hidden_comment() {
    let (server, client) = setup().await;
    let with_body = |n: u64, body: &str| {
        let mut issue = issue_json(n, "open");
        issue["body"] = json!(body);
        issue
    };
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .and(query_param(
            "q",
            r#"repo:o/r is:issue "twitch-1337-feedback:F42" in:body"#,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 4,
            "items": [
                {
                    "number": 6,
                    "html_url": "https://github.com/o/r/pull/6",
                    "state": "open",
                    "body": "<!-- twitch-1337-feedback:F42 -->",
                    "pull_request": { "url": "https://api.github.com/repos/o/r/pulls/6" },
                },
                // A viewer quoting the marker in another case, and another
                // entry whose marker merely starts with F42's.
                with_body(4, "```text\nTWITCH-1337-FEEDBACK:F42\n```\n<!-- twitch-1337-feedback:F5 -->"),
                with_body(5, "<!-- twitch-1337-feedback:F420 -->"),
                with_body(7, "---\n\n<!-- twitch-1337-feedback:F42 -->\n"),
            ],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .and(query_param(
            "q",
            r#"repo:o/r is:issue "twitch-1337-feedback:F43" in:comments"#,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 2,
            "items": [issue_json(8, "open"), issue_json(9, "open")],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/8/comments"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{ "body": "twitch 1337 feedback F43, lol" }])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/9/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "body": "unrelated" },
            { "body": "+1\n\n<!-- twitch-1337-feedback:F43 -->\n" },
        ])))
        .mount(&server)
        .await;

    assert_eq!(
        client
            .find_marker("twitch-1337-feedback:F42", MarkerScope::Body)
            .await
            .expect("search"),
        Some(7)
    );
    assert_eq!(
        client
            .find_marker("twitch-1337-feedback:F43", MarkerScope::Comments)
            .await
            .expect("search"),
        Some(9)
    );
}

#[test]
fn marker_query_strips_quotes() {
    assert_eq!(
        marker_query("o/r", r#"a" label:x "b"#, MarkerScope::Body),
        r#"repo:o/r is:issue "a label:x b" in:body"#
    );
}

#[tokio::test]
async fn duplicate_of_reads_graphql_duplicate_reference() {
    let (server, client) = setup().await;
    let gql = |number: u64, reply: Value| {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(json!({
                "variables": { "owner": "o", "name": "r", "number": number },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
    };
    gql(
        10,
        json!({ "data": { "repository": { "issue": {
            "duplicateOf": { "number": 3, "repository": { "nameWithOwner": "O/R" } }
        } } } }),
    )
    .mount(&server)
    .await;
    gql(
        11,
        json!({ "data": { "repository": { "issue": {
            "duplicateOf": { "number": 3, "repository": { "nameWithOwner": "other/repo" } }
        } } } }),
    )
    .mount(&server)
    .await;
    gql(
        12,
        json!({ "data": { "repository": { "issue": { "duplicateOf": null } } } }),
    )
    .mount(&server)
    .await;
    gql(
        13,
        json!({
            "data": { "repository": { "issue": null } },
            "errors": [{ "type": "NOT_FOUND", "message": "Could not resolve to an Issue with the number of 13." }],
        }),
    )
    .mount(&server)
    .await;

    assert_eq!(client.duplicate_of(10).await.expect("dup"), Some(3));
    assert_eq!(client.duplicate_of(11).await.expect("other repo"), None);
    assert_eq!(client.duplicate_of(12).await.expect("not dup"), None);
    assert!(matches!(
        client.duplicate_of(13).await,
        Err(GithubError::NotFound)
    ));
}

#[test]
fn both_key_formats_parse() {
    parse_private_key(TEST_PRIVATE_KEY_PEM).expect("pkcs1");
    parse_private_key(include_str!("fixtures/test_key_pkcs8.pem")).expect("pkcs8");
    assert!(matches!(
        parse_private_key("garbage"),
        Err(KeyError::Pem(_))
    ));
}

#[test]
fn repo_validation() {
    for good in ["o/r", "Chronophylos/twitch-1337", "a-b/c_d.e"] {
        assert!(is_valid_repo(good), "{good}");
    }
    for bad in ["", "o", "o/", "/r", "o/r/x", "o /r", "o/..", "o/r\n"] {
        assert!(!is_valid_repo(bad), "{bad:?}");
    }
}

#[test]
fn error_message_is_sanitized_and_truncated() {
    let long = format!("bad\u{1b}[31m\nthing {}", "x".repeat(300));
    let message = sanitize_message(&long);
    assert!(!message.chars().any(char::is_control), "{message:?}");
    assert_eq!(message.chars().count(), MESSAGE_MAX_CHARS);
    assert!(message.starts_with("bad [31m thing"));
    assert!(message.ends_with('…'));
}

#[test]
fn issue_url_points_at_github() {
    crate::install_crypto_provider();
    let client = GithubClient::new(&bootstrap("Chronophylos/twitch-1337")).expect("client");
    assert_eq!(
        client.issue_url(775),
        "https://github.com/Chronophylos/twitch-1337/issues/775"
    );
}
