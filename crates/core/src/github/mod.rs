//! GitHub App client for the feedback pipeline (#775): files issues, comments
//! and labels on one repository, finds feedback markers, and lists closes.
//!
//! Auth is a GitHub App: an RS256 JWT (hand-signed with `ring`) looks up the
//! repository's installation and mints an installation token, cached until
//! five minutes before it expires. Tokens and JWTs never reach a log line:
//! they ride in `bearer_auth` headers (marked sensitive) and live in
//! [`SecretString`]s.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use eyre::{WrapErr as _, bail};
use jiff::{SignedDuration, Timestamp};
use reqwest::header::{ACCEPT, LINK};
use reqwest::{Method, RequestBuilder, StatusCode, Url};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject as _;
use secrecy::{ExposeSecret as _, SecretString};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;
use tracing::instrument;

use crate::APP_USER_AGENT;
use crate::config::GithubBootstrap;
use crate::util::http::{BodyError, SendTraced as _, read_capped};

pub const DEFAULT_API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const ACCEPT_JSON: &str = "application/vnd.github+json";
const TIMEOUT: Duration = Duration::from_secs(15);
/// Body cap for a success. A page of 100 issues with long bodies is the
/// largest reply (issue bodies cap at 65 536 chars).
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;
/// Body cap for an error reply; only its `message` is used.
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;
const PER_PAGE: &str = "100";
/// `closed_since` stops after this many pages (5 000 issues). Results are
/// sorted by `updated` ascending, so a caller advancing its cursor to the
/// last returned issue picks up the rest on its next poll.
const MAX_PAGES: u32 = 50;
/// GitHub rejects a JWT whose `iat` is in the future; backdate for clock drift.
const JWT_BACKDATE_SECS: i64 = 60;
/// GitHub caps JWT lifetime at 10 minutes.
const JWT_LIFETIME_SECS: i64 = 9 * 60;
const TOKEN_REFRESH_MARGIN: SignedDuration = SignedDuration::from_mins(5);
/// Toast-sized cap on GitHub's error message.
const MESSAGE_MAX_CHARS: usize = 200;

/// GitHub's issue type names (org-level issue types), sent as `type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueType {
    Bug,
    Feature,
    Task,
}

impl IssueType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "Bug",
            Self::Feature => "Feature",
            Self::Task => "Task",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIssue {
    pub title: String,
    pub body: String,
    pub issue_type: IssueType,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueRef {
    pub number: u64,
    pub html_url: String,
}

/// Where [`GithubClient::find_marker`] looks for the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerScope {
    Body,
    Comments,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedIssue {
    pub number: u64,
    pub html_url: String,
    pub closed_at: Timestamp,
    pub updated_at: Timestamp,
    pub reason: CloseReason,
}

/// GitHub's `state_reason` of a closed issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Completed,
    NotPlanned,
    Duplicate,
    /// No reason recorded (issues closed before reasons existed) or one
    /// GitHub added later.
    Other,
}

impl CloseReason {
    fn from_wire(state_reason: Option<&str>) -> Self {
        match state_reason {
            Some("completed") => Self::Completed,
            Some("not_planned") => Self::NotPlanned,
            Some("duplicate") => Self::Duplicate,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// The request may have reached GitHub: transport failure (timeout,
    /// connection reset), 5xx, or an unreadable success reply. A write may
    /// have happened; the caller must not assume nothing was created.
    #[error("github request outcome is unknown")]
    Ambiguous(#[source] GithubCause),
    /// Clear failure before anything was created: 401/403/429 (incl. rate
    /// limits), or the JWT / installation token could not be obtained.
    #[error("github app authentication failed")]
    Auth(#[source] GithubCause),
    /// 404/410, or the number names a pull request.
    #[error("github resource not found")]
    NotFound,
    /// Any other 4xx (422 validation etc.). `message` is GitHub's `message`
    /// (plus the first validation error), control chars stripped, at most
    /// 200 chars: safe for a dashboard toast.
    #[error("github rejected the request with status {status}: {message}")]
    Rejected { status: u16, message: String },
}

/// Operational cause inside [`GithubError::Ambiguous`] / [`GithubError::Auth`].
#[derive(Debug, thiserror::Error)]
pub enum GithubCause {
    #[error("request failed")]
    Request(#[source] reqwest::Error),
    #[error("github returned {status}: {message}")]
    Status { status: StatusCode, message: String },
    #[error("response body could not be read")]
    Body(#[source] BodyError),
    #[error("response did not parse")]
    Parse(#[source] serde_json::Error),
    #[error("graphql error {kind}: {message}")]
    Graphql { kind: String, message: String },
    #[error("app JWT could not be signed")]
    Sign(#[source] ring::error::Unspecified),
    #[error("installation token could not be obtained")]
    Installation(#[source] Box<GithubError>),
}

/// Why a configured private key is unusable.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("no PEM private key found")]
    Pem(#[source] rustls_pki_types::pem::Error),
    #[error("PEM key is neither PKCS#1 nor PKCS#8")]
    UnsupportedFormat,
    #[error("key is not a usable RSA private key")]
    Rejected(#[source] ring::error::KeyRejected),
}

/// Parse a GitHub App private key: PKCS#1 (`BEGIN RSA PRIVATE KEY`, what
/// GitHub hands out) or PKCS#8 (`BEGIN PRIVATE KEY`). Shared with startup
/// validation so a bad key fails at boot, not at the first promote.
pub fn parse_private_key(pem: &str) -> Result<RsaKeyPair, KeyError> {
    match PrivateKeyDer::from_pem_slice(pem.as_bytes()).map_err(KeyError::Pem)? {
        PrivateKeyDer::Pkcs1(der) => RsaKeyPair::from_der(der.secret_pkcs1_der()),
        PrivateKeyDer::Pkcs8(der) => RsaKeyPair::from_pkcs8(der.secret_pkcs8_der()),
        _ => return Err(KeyError::UnsupportedFormat),
    }
    .map_err(KeyError::Rejected)
}

/// `owner/name` with GitHub's allowed characters and nothing else.
pub fn is_valid_repo(repo: &str) -> bool {
    let valid_part = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| valid_part(owner) && valid_part(name))
}

struct CachedToken {
    token: SecretString,
    refresh_at: Timestamp,
}

#[derive(Default)]
struct AuthState {
    installation_id: Option<u64>,
    token: Option<CachedToken>,
}

pub struct GithubClient {
    http: reqwest::Client,
    api_base: String,
    repo: String,
    app_id: u64,
    key: RsaKeyPair,
    rng: SystemRandom,
    /// Async lock held across the token fetch: concurrent callers wait for
    /// one fetch instead of minting a token each.
    auth: Mutex<AuthState>,
}

impl fmt::Debug for GithubClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubClient")
            .field("api_base", &self.api_base)
            .field("repo", &self.repo)
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

/// A 2xx reply: its body and whether a `Link: rel="next"` page follows.
struct Reply {
    body: Vec<u8>,
    has_next: bool,
}

impl GithubClient {
    /// Parse the key and build the HTTP client; no network.
    pub fn new(cfg: &GithubBootstrap) -> eyre::Result<Self> {
        if !is_valid_repo(&cfg.repo) {
            bail!("github.repo must be \"owner/name\", got {:?}", cfg.repo);
        }
        let key = parse_private_key(cfg.private_key.expose_secret())
            .wrap_err("github.private_key is not a usable RSA private key")?;
        let http = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .timeout(TIMEOUT)
            .build()
            .wrap_err("build github HTTP client")?;
        Ok(Self {
            http,
            api_base: DEFAULT_API_BASE.to_owned(),
            repo: cfg.repo.clone(),
            app_id: cfg.app_id,
            key,
            rng: SystemRandom::new(),
            auth: Mutex::new(AuthState::default()),
        })
    }

    /// Test hook: point the client at another API origin (e.g. wiremock).
    pub fn with_api_base(mut self, base: Url) -> Self {
        self.api_base = base.as_str().trim_end_matches('/').to_owned();
        self
    }

    pub fn issue_url(&self, number: u64) -> String {
        format!("https://github.com/{}/issues/{number}", self.repo)
    }

    #[instrument(skip_all, name = "github.create_issue", fields(github.issue.type = issue.issue_type.as_str()))]
    pub async fn create_issue(&self, issue: &NewIssue) -> Result<IssueRef, GithubError> {
        let body = json!({
            "title": issue.title,
            "body": issue.body,
            "type": issue.issue_type.as_str(),
            "labels": issue.labels,
        });
        let request = self
            .request(Method::POST, &format!("/repos/{}/issues", self.repo))
            .json(&body);
        let wire: WireIssue = parse(&self.api(request).await?.body)?;
        Ok(wire.into_ref())
    }

    /// 404/410 and pull requests are [`GithubError::NotFound`].
    #[instrument(skip_all, name = "github.get_issue", fields(github.issue.number = number))]
    pub async fn get_issue(&self, number: u64) -> Result<IssueRef, GithubError> {
        let request = self.request(
            Method::GET,
            &format!("/repos/{}/issues/{number}", self.repo),
        );
        let wire: WireIssue = parse(&self.api(request).await?.body)?;
        if wire.pull_request.is_some() {
            return Err(GithubError::NotFound);
        }
        Ok(wire.into_ref())
    }

    #[instrument(skip_all, name = "github.comment", fields(github.issue.number = number))]
    pub async fn comment(&self, number: u64, body: &str) -> Result<(), GithubError> {
        let request = self
            .request(
                Method::POST,
                &format!("/repos/{}/issues/{number}/comments", self.repo),
            )
            .json(&json!({ "body": body }));
        self.api(request).await.map(drop)
    }

    #[instrument(skip_all, name = "github.add_labels", fields(github.issue.number = number))]
    pub async fn add_labels(&self, number: u64, labels: &[&str]) -> Result<(), GithubError> {
        let request = self
            .request(
                Method::POST,
                &format!("/repos/{}/issues/{number}/labels", self.repo),
            )
            .json(&json!({ "labels": labels }));
        self.api(request).await.map(drop)
    }

    /// Search `"{marker}" in:body` or `in:comments` in this repo; the first
    /// issue that really carries the hidden `<!-- {marker} -->` comment the
    /// feedback renderer writes.
    ///
    /// Search ignores case and punctuation, so viewer text quoted in another
    /// issue (`TWITCH 1337 FEEDBACK F8`) can match the phrase; only the exact
    /// comment counts. Search is also eventually consistent: an issue created
    /// seconds ago may not be indexed yet.
    #[instrument(skip_all, name = "github.find_marker")]
    pub async fn find_marker(
        &self,
        marker: &str,
        scope: MarkerScope,
    ) -> Result<Option<u64>, GithubError> {
        let request = self.request(Method::GET, "/search/issues").query(&[
            ("q", marker_query(&self.repo, marker, scope).as_str()),
            ("per_page", "10"),
        ]);
        let wire: WireSearch = parse(&self.api(request).await?.body)?;
        let exact = format!("<!-- {marker} -->");
        for item in wire.items {
            if item.pull_request.is_some() {
                continue;
            }
            let carries = match scope {
                MarkerScope::Body => item.body.as_deref().is_some_and(|b| b.contains(&exact)),
                MarkerScope::Comments => self.comments_contain(item.number, &exact).await?,
            };
            if carries {
                return Ok(Some(item.number));
            }
        }
        Ok(None)
    }

    /// Whether any comment on issue `number` contains `needle`.
    async fn comments_contain(&self, number: u64, needle: &str) -> Result<bool, GithubError> {
        let path = format!("/repos/{}/issues/{number}/comments", self.repo);
        for page in 1..=MAX_PAGES {
            let request = self
                .request(Method::GET, &path)
                .query(&[("per_page", PER_PAGE)])
                .query(&[("page", page)]);
            let reply = self.api(request).await?;
            let comments: Vec<WireComment> = parse(&reply.body)?;
            if comments
                .iter()
                .any(|c| c.body.as_deref().is_some_and(|b| b.contains(needle)))
            {
                return Ok(true);
            }
            if !reply.has_next {
                break;
            }
        }
        Ok(false)
    }

    /// Closed issues (PRs excluded) updated at/after `since`, all pages (per_page=100, sort=updated, direction=asc).
    #[instrument(skip_all, name = "github.closed_since")]
    pub async fn closed_since(&self, since: Timestamp) -> Result<Vec<ClosedIssue>, GithubError> {
        let since = since.strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
        let path = format!("/repos/{}/issues", self.repo);
        let mut closed = Vec::new();
        for page in 1..=MAX_PAGES {
            let request = self
                .request(Method::GET, &path)
                .query(&[
                    ("state", "closed"),
                    ("since", since.as_str()),
                    ("sort", "updated"),
                    ("direction", "asc"),
                    ("per_page", PER_PAGE),
                ])
                .query(&[("page", page)]);
            let reply = self.api(request).await?;
            let issues: Vec<WireIssue> = parse(&reply.body)?;
            closed.extend(issues.into_iter().filter_map(WireIssue::into_closed));
            if !reply.has_next {
                return Ok(closed);
            }
        }
        tracing::warn!(
            pages = MAX_PAGES,
            "github closed-issue listing truncated; the next poll continues"
        );
        Ok(closed)
    }

    /// For an issue closed as duplicate: the canonical issue number, if it is
    /// in this repository.
    ///
    /// REST does not expose the canonical issue (`marked_as_duplicate`
    /// timeline events carry no reference), so this asks GraphQL for
    /// `Issue.duplicateOf`. `Ok(None)` when the issue is not marked as a
    /// duplicate or its canonical issue lives in another repository.
    #[instrument(skip_all, name = "github.duplicate_of", fields(github.issue.number = number))]
    pub async fn duplicate_of(&self, number: u64) -> Result<Option<u64>, GithubError> {
        let Some((owner, name)) = self.repo.split_once('/') else {
            return Ok(None);
        };
        let request = self.request(Method::POST, "/graphql").json(&json!({
            "query": DUPLICATE_QUERY,
            "variables": { "owner": owner, "name": name, "number": number },
        }));
        let reply: GqlReply = parse(&self.api(request).await?.body)?;
        let issue = reply
            .data
            .and_then(|data| data.repository)
            .and_then(|repo| repo.issue);
        if let Some(issue) = issue {
            return Ok(issue
                .duplicate_of
                .filter(|dup| {
                    dup.repository
                        .name_with_owner
                        .eq_ignore_ascii_case(&self.repo)
                })
                .map(|dup| dup.number));
        }
        match reply.errors.into_iter().next() {
            None => Err(GithubError::NotFound),
            Some(error) => Err(graphql_error(error)),
        }
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.api_base))
            .header(ACCEPT, ACCEPT_JSON)
            .header("X-GitHub-Api-Version", API_VERSION)
    }

    /// Send with the installation token. A 401 drops the cached token so
    /// the next call mints a fresh one; the failed call is not retried (it
    /// may be a write).
    async fn api(&self, request: RequestBuilder) -> Result<Reply, GithubError> {
        let token = self.installation_token().await?;
        let result = execute(request.bearer_auth(token.expose_secret())).await;
        if let Err(GithubError::Auth(GithubCause::Status { status, .. })) = &result
            && *status == StatusCode::UNAUTHORIZED
        {
            self.auth.lock().await.token = None;
        }
        result
    }

    async fn installation_token(&self) -> Result<SecretString, GithubError> {
        let mut auth = self.auth.lock().await;
        let now = Timestamp::now();
        if let Some(cached) = auth.token.as_ref()
            && now < cached.refresh_at
        {
            return Ok(cached.token.clone());
        }
        let jwt = self.app_jwt(now)?;
        let fresh = self
            .fetch_token(&mut auth, &jwt)
            .await
            .map_err(|error| GithubError::Auth(GithubCause::Installation(Box::new(error))))?;
        let token = fresh.token.clone();
        auth.token = Some(fresh);
        Ok(token)
    }

    async fn fetch_token(
        &self,
        auth: &mut AuthState,
        jwt: &SecretString,
    ) -> Result<CachedToken, GithubError> {
        let installation_id = match auth.installation_id {
            Some(id) => id,
            None => {
                let request = self
                    .request(Method::GET, &format!("/repos/{}/installation", self.repo))
                    .bearer_auth(jwt.expose_secret());
                let wire: WireInstallation = parse(&execute(request).await?.body)?;
                auth.installation_id = Some(wire.id);
                wire.id
            }
        };
        let request = self
            .request(
                Method::POST,
                &format!("/app/installations/{installation_id}/access_tokens"),
            )
            .bearer_auth(jwt.expose_secret());
        let wire: WireToken = match execute(request).await.and_then(|reply| parse(&reply.body)) {
            Ok(wire) => wire,
            Err(error) => {
                // The app may have been reinstalled under a new id.
                auth.installation_id = None;
                return Err(error);
            }
        };
        Ok(CachedToken {
            token: wire.token,
            // Out of range only for a nonsense `expires_at`; refresh next call.
            refresh_at: wire
                .expires_at
                .checked_sub(TOKEN_REFRESH_MARGIN)
                .unwrap_or(Timestamp::MIN),
        })
    }

    /// RS256 app JWT: `iat` backdated 60 s, `exp` 9 min out, `iss` = app id.
    fn app_jwt(&self, now: Timestamp) -> Result<SecretString, GithubError> {
        let iat = now.as_second() - JWT_BACKDATE_SECS;
        let exp = now.as_second() + JWT_LIFETIME_SECS;
        let mut jwt = BASE64URL.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
        jwt.push('.');
        BASE64URL.encode_string(
            format!(r#"{{"iat":{iat},"exp":{exp},"iss":{}}}"#, self.app_id),
            &mut jwt,
        );
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &self.rng, jwt.as_bytes(), &mut signature)
            .map_err(|error| GithubError::Auth(GithubCause::Sign(error)))?;
        jwt.push('.');
        BASE64URL.encode_string(&signature, &mut jwt);
        Ok(SecretString::from(jwt))
    }
}

/// Send and map the status; the body of a 2xx is read whole (capped).
async fn execute(request: RequestBuilder) -> Result<Reply, GithubError> {
    let response = request
        .send_traced()
        .await
        .map_err(|error| GithubError::Ambiguous(GithubCause::Request(error)))?;
    let status = response.status();
    if !status.is_success() {
        let body = match read_capped(response, MAX_ERROR_BODY_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                tracing::debug!(?error, %status, "github error body unreadable");
                Vec::new()
            }
        };
        return Err(status_error(status, &body));
    }
    let has_next = response
        .headers()
        .get_all(LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|link| link.contains(r#"rel="next""#));
    let body = read_capped(response, MAX_BODY_BYTES)
        .await
        .map_err(|error| GithubError::Ambiguous(GithubCause::Body(error)))?;
    Ok(Reply { body, has_next })
}

/// A 2xx body that does not parse is still a reply GitHub sent: a write
/// behind it may have happened, so it is ambiguous.
fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, GithubError> {
    serde_json::from_slice(body).map_err(|error| GithubError::Ambiguous(GithubCause::Parse(error)))
}

fn status_error(status: StatusCode, body: &[u8]) -> GithubError {
    let message = github_message(status, body);
    match status.as_u16() {
        401 | 403 | 429 => GithubError::Auth(GithubCause::Status { status, message }),
        404 | 410 => GithubError::NotFound,
        400..=499 => GithubError::Rejected {
            status: status.as_u16(),
            message,
        },
        _ => GithubError::Ambiguous(GithubCause::Status { status, message }),
    }
}

#[derive(Deserialize)]
struct WireErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    errors: Vec<WireValidationError>,
}

/// One entry of a 422's `errors`: an object, or (older endpoints) a string.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireValidationError {
    Text(String),
    Detail {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        field: Option<String>,
        #[serde(default)]
        code: Option<String>,
    },
}

/// GitHub's `message`, plus the first validation error, sanitized for a
/// toast. Falls back to the status reason.
fn github_message(status: StatusCode, body: &[u8]) -> String {
    let parsed: Option<WireErrorBody> = serde_json::from_slice(body).ok();
    let (message, detail) = match parsed {
        Some(body) => {
            let detail = body
                .errors
                .into_iter()
                .next()
                .and_then(|error| match error {
                    WireValidationError::Text(text) => Some(text),
                    WireValidationError::Detail {
                        message: Some(message),
                        ..
                    } => Some(message),
                    WireValidationError::Detail { field, code, .. } => match (field, code) {
                        (Some(field), Some(code)) => Some(format!("{field} {code}")),
                        (field, code) => field.or(code),
                    },
                });
            (body.message, detail)
        }
        None => (None, None),
    };
    let raw = match (message, detail) {
        (Some(message), Some(detail)) => format!("{message}: {detail}"),
        (Some(message), None) => message,
        (None, Some(detail)) => detail,
        (None, None) => status.canonical_reason().unwrap_or("error").to_owned(),
    };
    sanitize_message(&raw)
}

fn sanitize_message(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() <= MESSAGE_MAX_CHARS {
        return cleaned.to_owned();
    }
    let mut truncated: String = cleaned.chars().take(MESSAGE_MAX_CHARS - 1).collect();
    truncated.push('…');
    truncated
}

fn marker_query(repo: &str, marker: &str, scope: MarkerScope) -> String {
    let scope = match scope {
        MarkerScope::Body => "in:body",
        MarkerScope::Comments => "in:comments",
    };
    // A quote would end the phrase and let the rest act as qualifiers.
    let marker: String = marker.chars().filter(|&c| c != '"').collect();
    format!("repo:{repo} is:issue \"{marker}\" {scope}")
}

fn graphql_error(error: GqlError) -> GithubError {
    let message = sanitize_message(&error.message);
    match error.kind.as_deref() {
        Some("NOT_FOUND") => GithubError::NotFound,
        Some(kind @ ("FORBIDDEN" | "INSUFFICIENT_SCOPES" | "RATE_LIMITED")) => {
            GithubError::Auth(GithubCause::Graphql {
                kind: kind.to_owned(),
                message,
            })
        }
        // GraphQL answers errors with 200; report them as a rejection of
        // the request with that status.
        _ => GithubError::Rejected {
            status: StatusCode::OK.as_u16(),
            message,
        },
    }
}

#[derive(Deserialize)]
struct WireIssue {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(default)]
    closed_at: Option<Timestamp>,
    #[serde(default)]
    updated_at: Option<Timestamp>,
    /// Present (an object) only on pull requests.
    #[serde(default)]
    pull_request: Option<IgnoredAny>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct WireComment {
    #[serde(default)]
    body: Option<String>,
}

impl WireIssue {
    fn into_ref(self) -> IssueRef {
        IssueRef {
            number: self.number,
            html_url: self.html_url,
        }
    }

    fn into_closed(self) -> Option<ClosedIssue> {
        if self.pull_request.is_some() || self.state != "closed" {
            return None;
        }
        let closed_at = self.closed_at?;
        Some(ClosedIssue {
            number: self.number,
            html_url: self.html_url,
            closed_at,
            updated_at: self.updated_at.unwrap_or(closed_at),
            reason: CloseReason::from_wire(self.state_reason.as_deref()),
        })
    }
}

#[derive(Deserialize)]
struct WireSearch {
    #[serde(default)]
    items: Vec<WireIssue>,
}

#[derive(Deserialize)]
struct WireInstallation {
    id: u64,
}

#[derive(Deserialize)]
struct WireToken {
    token: SecretString,
    expires_at: Timestamp,
}

const DUPLICATE_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { \
     repository(owner: $owner, name: $name) { issue(number: $number) { \
     duplicateOf { number repository { nameWithOwner } } } } }";

#[derive(Deserialize)]
struct GqlReply {
    #[serde(default)]
    data: Option<GqlData>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Deserialize)]
struct GqlData {
    #[serde(default)]
    repository: Option<GqlRepository>,
}

#[derive(Deserialize)]
struct GqlRepository {
    #[serde(default)]
    issue: Option<GqlIssue>,
}

#[derive(Deserialize)]
struct GqlIssue {
    #[serde(default, rename = "duplicateOf")]
    duplicate_of: Option<GqlDuplicate>,
}

#[derive(Deserialize)]
struct GqlDuplicate {
    number: u64,
    repository: GqlRepoName,
}

#[derive(Deserialize)]
struct GqlRepoName {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
struct GqlError {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    message: String,
}

/// Test fixtures for building a client against wiremock.
#[cfg(any(test, feature = "testing"))]
pub mod test_support {
    use secrecy::SecretString;

    use crate::config::GithubBootstrap;

    /// Throwaway 2048-bit RSA key (PKCS#1), test-only.
    pub const TEST_PRIVATE_KEY_PEM: &str = include_str!("fixtures/test_key_pkcs1.pem");
    /// Installation id and token [`mount_auth`] answers with.
    pub const TEST_INSTALLATION_ID: u64 = 1;
    pub const TEST_TOKEN: &str = "ghs_test";

    /// Bootstrap for `repo` with app id 1 and [`TEST_PRIVATE_KEY_PEM`].
    pub fn bootstrap(repo: &str) -> GithubBootstrap {
        GithubBootstrap {
            repo: repo.to_owned(),
            app_id: 1,
            private_key: SecretString::from(TEST_PRIVATE_KEY_PEM),
        }
    }

    /// Mount the installation lookup and token mint for `repo`.
    #[cfg(test)]
    pub async fn mount_auth(server: &wiremock::MockServer, repo: &str) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/installation")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": TEST_INSTALLATION_ID })),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/app/installations/{TEST_INSTALLATION_ID}/access_tokens"
            )))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": TEST_TOKEN,
                "expires_at": "2099-01-01T00:00:00Z",
            })))
            .mount(server)
            .await;
    }
}

#[cfg(test)]
mod tests;
